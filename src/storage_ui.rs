//! Interactive day × port storage matrix, with one selection model for mouse
//! and keyboard and a background worker for both scans and storage changes.
use crate::logs::storage::{self, Action, CellKey, Inventory, ScanProgress, Size};
use crate::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scope {
    #[default]
    Cell,
    Row,
    Column,
}
impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::Cell => "cell",
            Self::Row => "row",
            Self::Column => "column",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Cell => Self::Row,
            Self::Row => Self::Column,
            Self::Column => Self::Cell,
        }
    }
}
enum Update {
    Progress(String),
    Indexing(ScanProgress),
    Scanned(Inventory),
    Finished(Inventory, String),
}
#[derive(Default)]
pub(crate) struct Storage {
    pub inventory: Inventory,
    cells: BTreeMap<CellKey, Size>,
    ports: Vec<String>,
    days: Vec<String>,
    row: usize,
    column: usize,
    row_offset: usize,
    column_offset: usize,
    scope: Scope,
    multi: bool,
    selected: BTreeSet<CellKey>,
    pending: Option<Action>,
    rx: Option<std::sync::mpsc::Receiver<Update>>,
    operation: bool,
    status: String,
    scan_progress: Option<ScanProgress>,
}
fn root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("marklogic-tui/logs")
}
impl Storage {
    fn install(&mut self, inventory: Inventory) {
        let current = self.current();
        self.cells = inventory.cells();
        self.ports = self
            .cells
            .keys()
            .map(|key| key.0.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let port_order = |port: &str| match port {
            "----" => (0, 0),
            "TASK" => (2, 0),
            _ => (1, port.parse::<u16>().unwrap_or(u16::MAX)),
        };
        self.ports
            .sort_by(|a, b| port_order(a).cmp(&port_order(b)).then(a.cmp(b)));
        self.days = self
            .cells
            .keys()
            .map(|key| key.1.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.row = current
            .as_ref()
            .and_then(|(port, _)| self.ports.iter().position(|p| p == port))
            .unwrap_or(0);
        self.column = current
            .as_ref()
            .and_then(|(_, day)| self.days.iter().position(|d| d == day))
            .unwrap_or_else(|| {
                self.days
                    .iter()
                    .rposition(|day| day != "Unknown")
                    .unwrap_or(0)
            });
        self.selected.retain(|key| self.cells.contains_key(key));
        self.inventory = inventory;
    }
    fn current(&self) -> Option<CellKey> {
        Some((
            self.ports.get(self.row)?.clone(),
            self.days.get(self.column)?.clone(),
        ))
    }
    fn targets(&self) -> BTreeSet<CellKey> {
        let Some((port, day)) = self.current() else {
            return BTreeSet::new();
        };
        self.cells
            .keys()
            .filter(|(p, d)| match self.scope {
                Scope::Cell => *p == port && *d == day,
                Scope::Row => *p == port,
                Scope::Column => *d == day,
            })
            .cloned()
            .collect()
    }
    fn select(&mut self) {
        let keys = self.targets();
        if !self.multi {
            self.selected = keys;
        } else if keys.iter().all(|key| self.selected.contains(key)) {
            self.selected.retain(|key| !keys.contains(key));
        } else {
            self.selected.extend(keys);
        }
    }
    fn selection_size(&self) -> Size {
        let mut size = Size::default();
        for key in &self.selected {
            if let Some(value) = self.cells.get(key) {
                size.add(*value);
            }
        }
        size
    }
    fn selection_sources(&self) -> usize {
        self.inventory
            .sources
            .iter()
            .filter(|source| {
                source.ranges.iter().any(|range| {
                    self.selected
                        .contains(&(source.port.clone(), range.day.clone()))
                })
            })
            .count()
    }
    pub fn blocks_navigation(&self) -> bool {
        self.pending.is_some()
    }
    pub fn footer(&self) -> String {
        format!(
            " Arrows: move · Space: select {} · s: scope · v: multi {} · c: clear\n a: archive · t: thaw · d: delete · r: refresh · Tab: tabs · q: close",
            self.scope.label(),
            if self.multi { "ON" } else { "off" }
        )
    }
}

impl App {
    pub fn refresh_storage(&mut self) {
        if self.system.storage.rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.system.storage.rx = Some(rx);
        self.system.storage.status = "Finding cached log sources…".into();
        self.system.storage.scan_progress = None;
        std::thread::spawn(move || {
            let data = storage::scan(&root(), |progress| {
                let _ = tx.send(Update::Indexing(progress));
            });
            let _ = tx.send(Update::Scanned(data));
        });
    }
    pub fn poll_storage(&mut self) {
        let mut complete = false;
        let mut disconnected = false;
        if let Some(rx) = &self.system.storage.rx {
            let mut updates = Vec::new();
            for _ in 0..64 {
                match rx.try_recv() {
                    Ok(update) => updates.push(update),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            for update in updates {
                match update {
                    Update::Progress(message) => {
                        self.system.storage.scan_progress = None;
                        self.system.storage.status = message;
                    }
                    Update::Indexing(progress) => {
                        self.system.storage.scan_progress = Some(progress)
                    }
                    Update::Scanned(data) => {
                        self.system.storage.scan_progress = None;
                        let warnings = data.warnings.len();
                        self.system.storage.install(data);
                        self.system.storage.status = if warnings > 0 {
                            format!("{warnings} source(s) could not be indexed; details below.")
                        } else {
                            "Ready · dates use log timestamps; explicit zones use UTC.".into()
                        };
                        complete = true;
                    }
                    Update::Finished(data, message) => {
                        self.system.storage.scan_progress = None;
                        self.system.storage.install(data);
                        self.system.storage.status = message;
                        complete = true;
                    }
                }
            }
        }
        if complete || disconnected {
            if self.system.storage.operation {
                self.server_logs.finish_storage_operation();
                self.system.storage.operation = false;
            }
            if disconnected && !complete {
                self.system.storage.scan_progress = None;
                self.system.storage.status =
                    "Storage worker stopped unexpectedly. Refresh to retry.".into();
            }
            self.system.storage.rx = None;
        }
    }
    pub fn handle_storage_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.poll_storage();
        if let Some(action) = self.system.storage.pending {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.system.storage.pending = None;
                    self.server_logs.begin_storage_operation();
                    let inventory = self.system.storage.inventory.clone();
                    let selected = self.system.storage.selected.clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.system.storage.rx = Some(rx);
                    self.system.storage.operation = true;
                    self.system.storage.status = format!("{} in progress…", action.label());
                    std::thread::spawn(move || {
                        let (changed, mut warnings) =
                            storage::apply(&inventory, &selected, action, |message| {
                                let _ = tx.send(Update::Progress(message));
                            });
                        let mut data = storage::scan(&root(), |progress| {
                            let _ = tx.send(Update::Indexing(progress));
                        });
                        warnings.extend(data.warnings);
                        data.warnings = warnings;
                        let message = format!(
                            "{} finished: {changed} source(s) changed, {} issue(s).",
                            action.label(),
                            data.warnings.len()
                        );
                        let _ = tx.send(Update::Finished(data, message));
                    });
                }
                KeyCode::Esc | KeyCode::Char('n') => self.system.storage.pending = None,
                _ => {}
            }
            return Ok(false);
        }
        let grid = &mut self.system.storage;
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => grid.column = grid.column.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => {
                grid.column = (grid.column + 1).min(grid.days.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => grid.row = grid.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                grid.row = (grid.row + 1).min(grid.ports.len().saturating_sub(1))
            }
            KeyCode::Home => {
                grid.row = 0;
                grid.column = 0;
            }
            KeyCode::End => {
                grid.row = grid.ports.len().saturating_sub(1);
                grid.column = grid.days.len().saturating_sub(1);
            }
            KeyCode::PageDown => grid.row = (grid.row + 5).min(grid.ports.len().saturating_sub(1)),
            KeyCode::PageUp => grid.row = grid.row.saturating_sub(5),
            KeyCode::Char('s') => grid.scope = grid.scope.next(),
            KeyCode::Char('v') => grid.multi = !grid.multi,
            KeyCode::Char(' ') | KeyCode::Enter => grid.select(),
            KeyCode::Char('R') => {
                grid.scope = Scope::Row;
                grid.select();
            }
            KeyCode::Char('C') => {
                grid.scope = Scope::Column;
                grid.select();
            }
            KeyCode::Char('*') => grid.selected = grid.cells.keys().cloned().collect(),
            KeyCode::Char('c') | KeyCode::Esc => {
                grid.selected.clear();
                grid.scope = Scope::Cell;
            }
            KeyCode::Char('r') => self.refresh_storage(),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Char('a') | KeyCode::Char('t') => {
                if grid.rx.is_some() {
                    grid.status = "Wait for the current storage task to finish.".into();
                } else if grid.selected.is_empty() {
                    grid.status = "Select cells, a row, or a column first (Space, R, C).".into();
                } else {
                    grid.pending = Some(match key.code {
                        KeyCode::Char('a') => Action::Archive,
                        KeyCode::Char('t') => Action::Thaw,
                        _ => Action::Delete,
                    });
                }
            }
            _ => {}
        }
        Ok(false)
    }
    pub fn select_storage_mouse(&mut self, row: Option<usize>, column: Option<usize>, multi: bool) {
        let grid = &mut self.system.storage;
        if grid.pending.is_some() {
            return;
        }
        if let Some(row) = row {
            grid.row = row;
        }
        if let Some(column) = column {
            grid.column = column;
        }
        grid.scope = match (row, column) {
            (Some(_), None) => Scope::Row,
            (None, Some(_)) => Scope::Column,
            _ => Scope::Cell,
        };
        let previous = grid.multi;
        grid.multi |= multi;
        if row.is_none() && column.is_none() {
            grid.selected = grid.cells.keys().cloned().collect();
        } else {
            grid.select();
        }
        grid.multi = previous;
    }
}

fn size_lines(size: Size) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            format!("T {:>10}", crate::logs::format_bytes(size.thawed)),
            Style::default().fg(Color::LightGreen),
        ),
        Line::styled(
            format!(
                "F {:>10}{}",
                crate::logs::format_bytes(size.frozen),
                if size.estimated { "~" } else { "" }
            ),
            Style::default().fg(Color::LightCyan),
        ),
    ]
}
fn sum<'a>(items: impl Iterator<Item = &'a Size>) -> Size {
    let mut size = Size::default();
    for item in items {
        size.add(*item);
    }
    size
}
fn cell_rect(area: Rect, x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + x.min(area.width),
        area.y + y.min(area.height),
        width.min(area.width.saturating_sub(x)),
        height.min(area.height.saturating_sub(y)),
    )
}
fn draw_cell(f: &mut Frame, rect: Rect, lines: Vec<Line<'static>>, style: Style, focused: bool) {
    let border = ratatui::symbols::border::Set {
        bottom_right: "┼",
        bottom_left: "─",
        ..ratatui::symbols::border::PLAIN
    };
    f.render_widget(
        Paragraph::new(lines).style(style).block(
            Block::default()
                .borders(Borders::RIGHT | Borders::BOTTOM)
                .border_set(border)
                .border_style(Style::default().fg(if focused {
                    Color::White
                } else {
                    Color::DarkGray
                })),
        ),
        rect,
    );
}
fn review_message(grid: &Storage, action: Action, size: Size) -> String {
    let ports = grid
        .selected
        .iter()
        .map(|(port, _)| port.as_str())
        .collect::<BTreeSet<_>>();
    let ports_label = ports.iter().take(6).copied().collect::<Vec<_>>().join(", ");
    let days = grid
        .selected
        .iter()
        .map(|(_, day)| day.as_str())
        .collect::<BTreeSet<_>>();
    let date = |day: &str| {
        chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .map(|d| d.format("%d %b %Y").to_string())
            .unwrap_or_else(|_| day.to_owned())
    };
    let days_label = if days.len() <= 3 {
        days.iter()
            .map(|day| date(day))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!(
            "{} – {} ({} selected dates)",
            date(days.first().unwrap()),
            date(days.last().unwrap()),
            days.len()
        )
    };
    format!(
        "{} selected storage\nAll local environments and hosts\n{} cells across {} source files\nPorts: {}{}\nDates: {}\n\nStored: {}  ·  Thawed: {}  ·  Frozen: {}\nUncompressed content: {}\n\n{}\n\nEnter / y: apply     Esc / n: cancel",
        action.label(),
        grid.selected.len(),
        grid.selection_sources(),
        ports_label,
        if ports.len() > 6 {
            format!(" (+{} more)", ports.len() - 6)
        } else {
            String::new()
        },
        days_label,
        crate::logs::format_bytes(size.total()),
        crate::logs::format_bytes(size.thawed),
        crate::logs::format_bytes(size.frozen),
        crate::logs::format_bytes(size.logical),
        match action {
            Action::Delete => "Permanently removes only the selected local days and ports.",
            Action::Archive => "Compresses selected thawed content. Logs remain readable.",
            Action::Thaw => "Expands selected frozen content to ordinary local files.",
        }
    )
}

fn indexing_lines(progress: &ScanProgress) -> Vec<Line<'static>> {
    let percent = if progress.total_bytes > 0 {
        ((u128::from(progress.accounted_bytes) * 100 / u128::from(progress.total_bytes)).min(100))
            as usize
    } else if progress.total_sources > 0 {
        progress.completed_sources.saturating_mul(100) / progress.total_sources
    } else {
        0
    };
    let filled = percent * 12 / 100;
    let skipped = if progress.failed_sources == 0 {
        String::new()
    } else {
        format!(" · {} failed", progress.failed_sources)
    };
    let overall = format!(
        " Indexing [{}{}] {percent:>3}% · {}/{} sources · {} / {} uncompressed{skipped}",
        "█".repeat(filled),
        "░".repeat(12 - filled),
        progress.completed_sources,
        progress.total_sources,
        crate::logs::format_bytes(progress.accounted_bytes),
        crate::logs::format_bytes(progress.total_bytes),
    );
    let current = match &progress.current {
        Some(file) => format!(
            " Current: {} / {} · {} / {}",
            crate::logs::format_bytes(file.read_bytes),
            crate::logs::format_bytes(file.total_bytes),
            file.host,
            file.name
        ),
        None => " Preparing next source…".into(),
    };
    vec![
        Line::styled(overall, Style::default().fg(Color::Cyan)),
        Line::styled(current, Style::default().fg(Color::Gray)),
    ]
}

pub(crate) fn render(f: &mut Frame, app: &mut App, area: Rect) {
    app.poll_storage();
    let grid = &mut app.system.storage;
    let panels = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(5),
    ])
    .split(area);
    let total = sum(grid.cells.values());
    let summary = Line::from(vec![
        Span::styled(
            format!(
                " Local logs · {} ports × {} days · ",
                grid.ports.len(),
                grid.days.len()
            ),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("T {} thawed", crate::logs::format_bytes(total.thawed)),
            Style::default().fg(Color::LightGreen),
        ),
        Span::raw(" · "),
        Span::styled(
            format!("F {} frozen", crate::logs::format_bytes(total.frozen)),
            Style::default().fg(Color::LightCyan),
        ),
    ]);
    let header = match &grid.scan_progress {
        Some(progress) => indexing_lines(progress),
        None => vec![
            summary,
            Line::styled(
                format!(" {}", grid.status),
                Style::default().fg(Color::Gray),
            ),
        ],
    };
    f.render_widget(Paragraph::new(header), panels[0]);
    let block = Block::default().borders(Borders::ALL).title(format!(
        " Dates × ports · select {} · multi {} · {} selected ",
        grid.scope.label(),
        if grid.multi { "ON" } else { "off" },
        grid.selected.len()
    ));
    let inner = block.inner(panels[1]);
    f.render_widget(block, panels[1]);
    if grid.cells.is_empty() {
        f.render_widget(
            Paragraph::new(if grid.rx.is_some() {
                "Indexing stored records…"
            } else {
                "No cached log records. Download logs in :logs, then refresh."
            })
            .wrap(Wrap { trim: false }),
            inner,
        );
    } else if inner.width < 45 || inner.height < 9 {
        f.render_widget(
            Paragraph::new("Enlarge the pane to show the storage grid."),
            inner,
        );
    } else {
        const LABEL: u16 = 11;
        const MIN_WIDTH: u16 = 17;
        let columns = ((inner.width.saturating_sub(LABEL + MIN_WIDTH)) / MIN_WIDTH).max(1) as usize;
        let rows = (inner.height.saturating_sub(6) / 3).max(1) as usize;
        grid.column_offset = grid.column_offset.min(grid.column);
        if grid.column >= grid.column_offset + columns {
            grid.column_offset = grid.column + 1 - columns;
        }
        grid.row_offset = grid.row_offset.min(grid.row);
        if grid.row >= grid.row_offset + rows {
            grid.row_offset = grid.row + 1 - rows;
        }
        let visible_days = grid
            .days
            .iter()
            .enumerate()
            .skip(grid.column_offset)
            .take(columns)
            .collect::<Vec<_>>();
        let width = (inner.width - LABEL) / (visible_days.len() as u16 + 1);
        let total_x = LABEL + visible_days.len() as u16 * width;
        let corner = cell_rect(inner, 0, 0, LABEL, 3);
        draw_cell(
            f,
            corner,
            vec![Line::raw("Port / day"), Line::raw("Select all")],
            Style::default().fg(Color::Gray),
            false,
        );
        app.mouse_regions
            .push((corner, MouseTarget::StorageSelect(None, None)));
        for (display, (column, day)) in visible_days.iter().enumerate() {
            let rect = cell_rect(inner, LABEL + display as u16 * width, 0, width, 3);
            let label = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .map(|day| {
                    vec![
                        Line::raw(day.format("%a %d %b").to_string()).centered(),
                        Line::raw(day.format("%Y").to_string()).centered(),
                    ]
                })
                .unwrap_or_else(|_| vec![Line::raw((*day).clone()).centered()]);
            let selected = grid
                .cells
                .keys()
                .filter(|(_, date)| date == *day)
                .all(|key| grid.selected.contains(key));
            draw_cell(
                f,
                rect,
                label,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
                    .bg(if selected { Color::Blue } else { Color::Reset }),
                false,
            );
            app.mouse_regions
                .push((rect, MouseTarget::StorageSelect(None, Some(*column))));
        }
        draw_cell(
            f,
            cell_rect(inner, total_x, 0, inner.width - total_x, 3),
            vec![
                Line::raw("Port total").centered(),
                Line::raw("All dates").centered(),
            ],
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            false,
        );
        let shown_rows = grid.ports.len().saturating_sub(grid.row_offset).min(rows);
        for (display_row, (row, port)) in grid
            .ports
            .iter()
            .enumerate()
            .skip(grid.row_offset)
            .take(rows)
            .enumerate()
        {
            let y = 3 + display_row as u16 * 3;
            let selected = grid
                .cells
                .keys()
                .filter(|(p, _)| p == port)
                .all(|key| grid.selected.contains(key));
            let rect = cell_rect(inner, 0, y, LABEL, 3);
            draw_cell(
                f,
                rect,
                vec![Line::raw(format!(
                    "{}{}",
                    if selected { "✓ " } else { "  " },
                    if port == "----" { "System" } else { port }
                ))],
                Style::default()
                    .fg(Color::White)
                    .bg(if selected { Color::Blue } else { Color::Reset })
                    .add_modifier(Modifier::BOLD),
                false,
            );
            app.mouse_regions
                .push((rect, MouseTarget::StorageSelect(Some(row), None)));
            for (display_column, (column, day)) in visible_days.iter().enumerate() {
                let key = (port.clone(), (*day).clone());
                let focused = grid.row == row && grid.column == *column;
                let bg = if grid.selected.contains(&key) {
                    Color::Blue
                } else if focused {
                    Color::DarkGray
                } else {
                    Color::Reset
                };
                let rect = cell_rect(inner, LABEL + display_column as u16 * width, y, width, 3);
                let text = grid
                    .cells
                    .get(&key)
                    .map(|size| size_lines(*size))
                    .unwrap_or_else(|| {
                        vec![Line::styled(
                            "       —",
                            Style::default().fg(Color::DarkGray),
                        )]
                    });
                draw_cell(f, rect, text, Style::default().bg(bg), focused);
                app.mouse_regions
                    .push((rect, MouseTarget::StorageSelect(Some(row), Some(*column))));
            }
            let size = sum(grid
                .cells
                .iter()
                .filter(|((p, _), _)| p == port)
                .map(|(_, size)| size));
            draw_cell(
                f,
                cell_rect(inner, total_x, y, inner.width - total_x, 3),
                size_lines(size),
                Style::default().add_modifier(Modifier::BOLD),
                false,
            );
        }
        let y = (3 + shown_rows as u16 * 3).min(inner.height.saturating_sub(3));
        draw_cell(
            f,
            cell_rect(inner, 0, y, LABEL, 3),
            vec![Line::raw("Day totals"), Line::raw("All ports")],
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            false,
        );
        for (display, (_, day)) in visible_days.iter().enumerate() {
            let size = sum(grid
                .cells
                .iter()
                .filter(|((_, d), _)| d == *day)
                .map(|(_, size)| size));
            draw_cell(
                f,
                cell_rect(inner, LABEL + display as u16 * width, y, width, 3),
                size_lines(size),
                Style::default().add_modifier(Modifier::BOLD),
                false,
            );
        }
        draw_cell(
            f,
            cell_rect(inner, total_x, y, inner.width - total_x, 3),
            size_lines(total),
            Style::default().add_modifier(Modifier::BOLD),
            false,
        );
    }
    let selection = grid.selection_size();
    let mut details = vec![
        Line::from(vec![
            Span::styled(" T thawed ", Style::default().fg(Color::LightGreen)),
            Span::styled(" F frozen ", Style::default().fg(Color::LightCyan)),
            Span::styled(
                " blue background: selected ",
                Style::default().bg(Color::Blue).fg(Color::White),
            ),
            Span::raw(" · ~ estimated legacy compression"),
        ]),
        Line::raw(format!(
            " Selection: {} cells · {} sources · {} stored (T {} / F {})",
            grid.selected.len(),
            grid.selection_sources(),
            crate::logs::format_bytes(selection.total()),
            crate::logs::format_bytes(selection.thawed),
            crate::logs::format_bytes(selection.frozen)
        )),
    ];
    if let Some((port, day)) = grid.current() {
        let sources = grid
            .inventory
            .sources
            .iter()
            .filter(|source| {
                source.port == port && source.ranges.iter().any(|range| range.day == day)
            })
            .collect::<Vec<_>>();
        let names = sources
            .iter()
            .take(3)
            .map(|source| format!("{} / {}", source.host, source.name))
            .collect::<Vec<_>>()
            .join(" · ");
        details.push(Line::raw(format!(
            " {day} · port {port}: {names}{}",
            if sources.len() > 3 {
                format!(" · +{} sources", sources.len() - 3)
            } else {
                String::new()
            }
        )));
    }
    details.push(Line::styled(
        " All local environments · payload sizes exclude indexes and retired generations.",
        Style::default().fg(Color::DarkGray),
    ));
    if let Some(warning) = grid.inventory.warnings.first() {
        details.push(Line::styled(
            format!(" {}", warning),
            Style::default().fg(Color::LightRed),
        ));
    }
    f.render_widget(
        Paragraph::new(details).wrap(Wrap { trim: false }),
        panels[2],
    );
    if let Some(action) = grid.pending {
        let width = f.area().width.min(78);
        let message = review_message(grid, action, selection);
        let content_width = usize::from(width.saturating_sub(2).max(1));
        let lines: usize = message
            .lines()
            .map(|line| {
                unicode_width::UnicodeWidthStr::width(line)
                    .max(1)
                    .div_ceil(content_width)
            })
            .sum();
        let height = f
            .area()
            .height
            .min(lines.saturating_add(2).min(u16::MAX as usize) as u16);
        let rect = Rect::new(
            f.area().x + f.area().width.saturating_sub(width) / 2,
            f.area().y + f.area().height.saturating_sub(height) / 2,
            width,
            height,
        );
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(message).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Review storage action ")
                    .border_style(Style::default().fg(if action == Action::Delete {
                        Color::LightRed
                    } else {
                        Color::Cyan
                    })),
            ),
            rect,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::storage::{Range, Source};
    use ratatui::{Terminal, backend::TestBackend};

    fn grid() -> Storage {
        let mut inventory = Inventory::default();
        for port in ["8000", "9000"] {
            inventory.sources.push(Source {
                root: PathBuf::from("/unused"),
                metadata: PathBuf::from("/unused/metadata.json"),
                port: port.into(),
                host: format!("host-{port}"),
                name: format!("{port}_ErrorLog.txt"),
                ranges: vec![
                    Range {
                        day: "2026-10-27".into(),
                        offset: 0,
                        bytes: 4096,
                        frozen: true,
                        stored: 512,
                        estimated: false,
                    },
                    Range {
                        day: "2026-10-28".into(),
                        offset: 4096,
                        bytes: 8192,
                        frozen: false,
                        stored: 8192,
                        estimated: false,
                    },
                    Range {
                        day: "2026-10-29".into(),
                        offset: 12288,
                        bytes: 16384,
                        frozen: false,
                        stored: 16384,
                        estimated: false,
                    },
                ],
            });
        }
        let mut result = Storage::default();
        result.install(inventory);
        result.column = 0;
        result
    }
    fn app() -> App {
        let mut app = crate::tests::test_app();
        app.open_system();
        app.system.storage_tab = true;
        app.system.storage = grid();
        app
    }
    fn key(app: &mut App, code: KeyCode) {
        app.handle_system_key(KeyEvent::new(code, KeyModifiers::NONE))
            .unwrap();
    }
    #[test]
    fn row_column_cell_and_multiple_selection_use_the_same_cells() {
        let mut app = app();
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(
            app.system.storage.selected,
            BTreeSet::from([("8000".into(), "2026-10-27".into())])
        );
        key(&mut app, KeyCode::Char('C'));
        assert_eq!(app.system.storage.selected.len(), 2);
        assert_eq!(app.system.storage.selection_size().frozen, 1024);
        key(&mut app, KeyCode::Char('R'));
        assert_eq!(app.system.storage.selected.len(), 3);
        key(&mut app, KeyCode::Char('v'));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char('R'));
        assert_eq!(app.system.storage.selected.len(), 6);
        key(&mut app, KeyCode::Char('R'));
        assert_eq!(
            app.system.storage.selected.len(),
            3,
            "toggle removes the selected row"
        );
        key(&mut app, KeyCode::Char('c'));
        assert!(app.system.storage.selected.is_empty());
        assert_eq!(app.system.storage.scope, Scope::Cell);
    }
    #[test]
    fn mouse_headers_select_complete_rows_columns_and_ctrl_adds_cells() {
        let mut app = app();
        app.select_storage_mouse(Some(1), None, false);
        assert_eq!(app.system.storage.selected.len(), 3);
        app.select_storage_mouse(None, Some(0), false);
        assert_eq!(app.system.storage.selected.len(), 2);
        app.select_storage_mouse(Some(1), Some(1), true);
        assert_eq!(app.system.storage.selected.len(), 3);
        app.select_storage_mouse(Some(1), Some(1), true);
        assert_eq!(app.system.storage.selected.len(), 2);
        app.select_storage_mouse(None, None, false);
        assert_eq!(app.system.storage.selected.len(), 6);
    }
    #[test]
    fn actions_require_selection_and_show_review_before_changing_data() {
        let mut app = app();
        key(&mut app, KeyCode::Char('d'));
        assert!(app.system.storage.pending.is_none());
        for (code, action) in [
            ('a', Action::Archive),
            ('t', Action::Thaw),
            ('d', Action::Delete),
        ] {
            key(&mut app, KeyCode::Char(' '));
            key(&mut app, KeyCode::Char(code));
            assert_eq!(app.system.storage.pending, Some(action));
            assert!(!app.system.storage.operation);
            key(&mut app, KeyCode::Esc);
            assert!(app.system.storage.pending.is_none());
        }
    }
    #[test]
    fn indexing_progress_shows_total_work_and_current_file() {
        let mut app = app();
        app.system.storage.scan_progress = Some(ScanProgress {
            completed_sources: 2,
            total_sources: 5,
            failed_sources: 1,
            accounted_bytes: 40,
            total_bytes: 100,
            current: Some(storage::ScanFileProgress {
                host: "host-a".into(),
                name: "8000_ErrorLog.txt".into(),
                read_bytes: 30,
                total_bytes: 60,
            }),
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("40%"));
        assert!(text.contains("2/5 sources"));
        assert!(text.contains("40 B / 100 B uncompressed"));
        assert!(text.contains("1 failed"));
        assert!(text.contains("Current: 30 B / 60 B · host-a / 8000_ErrorLog.txt"));
    }

    #[test]
    fn review_shows_selected_ports_dates_and_confirmation_at_narrow_widths() {
        let mut app = app();
        key(&mut app, KeyCode::Char('C'));
        key(&mut app, KeyCode::Char('d'));
        for width in [120, 80, 45] {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Ports: 8000, 9000"));
            assert!(text.contains("Dates: 27 Oct 2026"));
            assert!(text.contains("Enter / y: apply"));
            assert!(!app.system.storage.operation);
        }
        key(&mut app, KeyCode::Esc);
        assert!(app.system.storage.pending.is_none());
    }

    #[test]
    fn renders_date_headers_totals_states_and_selection_without_overflow() {
        let mut app = app();
        key(&mut app, KeyCode::Char(' '));
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Tue 27 Oct"));
        assert!(text.contains("Wed 28 Oct"));
        assert!(text.contains("Thu 29 Oct"));
        assert!(text.contains("Port total"));
        assert!(text.contains("Day totals"));
        assert!(text.contains("24.0 KiB"));
        assert!(text.contains("512 B"));
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.fg == Color::LightGreen)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.fg == Color::LightCyan)
        );
        assert!(buffer.content.iter().any(|cell| cell.bg == Color::Blue));
        if std::env::var_os("STORAGE_PREVIEW").is_some() {
            for y in 0..buffer.area.height {
                println!(
                    "{}",
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                );
            }
        }
        for (width, height) in [(80, 24), (45, 12), (12, 4), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        }
    }
}
