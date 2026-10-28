use crate::{
    App, AppMode, KeyCode, KeyEvent, KeyModifiers,
    reports::{self, Definition, Entry, MemoryHistory},
};
use anyhow::{Context, Result};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    symbols::Marker,
    text::Line,
    widgets::{
        Axis, Block, Borders, Cell, Chart, Clear, Dataset, GraphType, List, ListItem, ListState,
        Paragraph, Row, Table, Wrap,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::mpsc,
};

#[derive(Default)]
pub(crate) struct Reports {
    root: PathBuf,
    entries: Vec<Entry>,
    selected: ListState,
    cache: Option<PathBuf>,
    directory: String,
    pub form: Option<Form>,
    pub files_focus: bool,
    output: Option<MemoryHistory>,
    metric: usize,
    scroll: usize,
    message: String,
    running: Option<mpsc::Receiver<Result<MemoryHistory>>>,
    result_environment: String,
}

pub(crate) struct Form {
    definition: Definition,
    values: Vec<String>,
    selected: usize,
    error: String,
}

impl Reports {
    fn refresh(&mut self) -> Result<()> {
        let selected_path = self
            .selected
            .selected()
            .and_then(|i| self.entries.get(i))
            .map(|e| e.path.clone());
        self.entries = reports::discover(&self.root)?;
        self.output = None;
        self.scroll = 0;
        let index = selected_path
            .and_then(|path| self.entries.iter().position(|e| e.path == path))
            .unwrap_or(0);
        self.selected
            .select((!self.entries.is_empty()).then_some(index));
        Ok(())
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selected
            .selected()
            .and_then(|i| self.entries.get(i))
            .map(|e| e.path.clone())
    }

    fn select(&mut self, index: usize) {
        if self.entries.is_empty() || self.running.is_some() {
            return;
        }
        let index = index.min(self.entries.len() - 1);
        if self.selected.selected() != Some(index) {
            self.selected.select(Some(index));
            self.output = None;
            self.scroll = 0;
            self.message.clear();
        }
    }

    fn metrics(&self) -> Vec<String> {
        let mut metrics = vec!["used".into()];
        if let Some(output) = &self.output {
            let keys: BTreeSet<_> = output
                .samples
                .iter()
                .flat_map(|s| s.percentages.keys().cloned())
                .collect();
            metrics.extend(keys.into_iter().filter(|key| key != "used"));
        }
        metrics
    }

    pub fn poll(&mut self) {
        let Some(receiver) = &self.running else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.running = None;
                match result {
                    Ok(output) => {
                        self.message = format!(
                            "{} samples from {} cached files · server-local timestamps · cached data may have gaps{}{}",
                            output.samples.len(),
                            output.files,
                            if output.malformed > 0 {
                                format!(" · {} malformed memory records skipped", output.malformed)
                            } else {
                                String::new()
                            },
                            if !output.warnings.is_empty() {
                                format!(
                                    " · {} cache warning(s): {}",
                                    output.warnings.len(),
                                    output.warnings[0]
                                )
                            } else {
                                String::new()
                            }
                        );
                        self.output = Some(output);
                        self.scroll = 0;
                        self.metric = 0;
                    }
                    Err(error) => self.message = format!("Report failed: {error:#}"),
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.running = None;
                self.message = "Report worker stopped before returning a result".into();
            }
        }
    }
}

impl App {
    pub(crate) fn open_reports(&mut self, directory: &str) {
        self.reports.root = crate::config::AppConfig::config_path()
            .parent()
            .expect("config directory")
            .join("reports");
        self.reports.directory = directory.into();
        self.reconcile_report_source();
        self.reports.files_focus = true;
        if self.reports.running.is_none() {
            if let Err(error) = self.reports.refresh() {
                self.reports.message = format!("Reports: {error:#}");
            }
        }
        self.mode = AppMode::Reports;
    }

    pub(crate) fn reconcile_report_source(&mut self) {
        let cache = self
            .client
            .as_ref()
            .map(|client| crate::logs::report_cache_root(client, &self.reports.directory));
        if self.reports.cache != cache {
            self.reports.cache = cache;
            // Drop the receiver so an old server's completion cannot replace new server data.
            self.reports.running = None;
            self.reports.output = None;
            self.reports.form = None;
            self.reports.message =
                "Choose a report and press r to run against this server's downloaded logs.".into();
        }
    }

    fn prepare_report(&mut self) -> Result<()> {
        if self.reports.running.is_some() {
            return Ok(());
        }
        let path = self.reports.selected_path().context("No report selected")?;
        // Always reread from disk; edits made outside the app are picked up on every run.
        let definition = Definition::load(&path)?;
        let values = definition
            .inputs
            .iter()
            .map(|input| input.default.clone())
            .collect();
        self.reports.form = Some(Form {
            definition,
            values,
            selected: 0,
            error: String::new(),
        });
        if self
            .reports
            .form
            .as_ref()
            .is_some_and(|form| form.values.is_empty())
        {
            self.start_report()?;
        }
        Ok(())
    }

    fn start_report(&mut self) -> Result<()> {
        let cache = self
            .reports
            .cache
            .clone()
            .context("Choose a server with :servers first")?;
        let form = self.reports.form.as_ref().context("No report inputs")?;
        let values = form
            .definition
            .inputs
            .iter()
            .zip(&form.values)
            .map(|(input, value)| (input.id.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        form.definition.validate_values(&values)?;
        let definition = form.definition.clone();
        let (sender, receiver) = mpsc::channel();
        self.rt.spawn_blocking(move || {
            let _ = sender.send(reports::execute(&definition, &values, &cache));
        });
        self.reports.running = Some(receiver);
        self.reports.form = None;
        self.reports.output = None;
        self.reports.result_environment = self.environment_indicator();
        self.reports.message = "Running report against downloaded logs…".into();
        Ok(())
    }

    pub(crate) fn handle_reports_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.reports.form.is_some() {
            if key.code == KeyCode::Enter {
                if let Err(error) = self.start_report() {
                    if let Some(form) = &mut self.reports.form {
                        form.error = format!("{error:#}");
                    }
                }
                return Ok(false);
            }
            let form = self.reports.form.as_mut().expect("open form");
            let count = form.values.len();
            match key.code {
                KeyCode::Esc => self.reports.form = None,
                KeyCode::Tab | KeyCode::Down if count > 0 => {
                    form.selected = (form.selected + 1) % count
                }
                KeyCode::BackTab | KeyCode::Up if count > 0 => {
                    form.selected = (form.selected + count - 1) % count
                }
                KeyCode::Backspace if count > 0 => {
                    form.values[form.selected].pop();
                }
                KeyCode::Char('u')
                    if key.modifiers.contains(KeyModifiers::CONTROL) && count > 0 =>
                {
                    form.values[form.selected].clear()
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && count > 0 =>
                {
                    form.values[form.selected].push(ch)
                }
                _ => {}
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.enter_start_page(),
            KeyCode::Char(':') => {
                self.mode = AppMode::Normal;
                self.focus_command_panel();
                self.command_input = ":".into();
                self.update_autocomplete();
            }
            KeyCode::Tab | KeyCode::BackTab => self.reports.files_focus = !self.reports.files_focus,
            KeyCode::Char('r') | KeyCode::F(5) => {
                if let Err(error) = self.prepare_report() {
                    self.reports.message = format!("Report: {error:#}");
                }
            }
            KeyCode::Char('e') if self.reports.running.is_none() => {
                if let Some(path) = self.reports.selected_path() {
                    let result = crate::external_editor::open_file_in_external_editor(&path);
                    self.needs_terminal_refresh = true;
                    self.reports.output = None;
                    self.reports.message = match result {
                        Ok(()) => format!("Saved {}; r: run", path.display()),
                        Err(error) => format!("Editor: {error:#}"),
                    };
                    if let Err(error) = self.reports.refresh() {
                        self.reports.message = format!("Reports: {error:#}");
                    }
                }
            }
            KeyCode::Char('R') if self.reports.running.is_none() => {
                if let Err(error) = self.reports.refresh() {
                    self.reports.message = format!("Reports: {error:#}");
                }
            }
            KeyCode::Char('m') => {
                self.reports.metric = (self.reports.metric + 1) % self.reports.metrics().len();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.reports.files_focus {
                    self.reports
                        .select(self.reports.selected.selected().unwrap_or(0) + 1);
                } else {
                    self.reports.scroll = self.reports.scroll.saturating_add(1).min(
                        self.reports
                            .output
                            .as_ref()
                            .map_or(0, |o| o.samples.len().saturating_sub(1)),
                    );
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.reports.files_focus {
                    self.reports.select(
                        self.reports
                            .selected
                            .selected()
                            .unwrap_or(0)
                            .saturating_sub(1),
                    );
                } else {
                    self.reports.scroll = self.reports.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                self.reports.scroll = self.reports.scroll.saturating_add(10).min(
                    self.reports
                        .output
                        .as_ref()
                        .map_or(0, |o| o.samples.len().saturating_sub(1)),
                )
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                self.reports.scroll = self.reports.scroll.saturating_sub(10)
            }
            KeyCode::Char('g') => {
                if self.reports.files_focus {
                    self.reports.select(0);
                } else {
                    self.reports.scroll = 0;
                }
            }
            KeyCode::Char('G') => {
                if self.reports.files_focus {
                    self.reports
                        .select(self.reports.entries.len().saturating_sub(1));
                } else {
                    self.reports.scroll = self
                        .reports
                        .output
                        .as_ref()
                        .map_or(0, |o| o.samples.len().saturating_sub(1));
                }
            }
            _ => {}
        }
        Ok(false)
    }
}

const COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Yellow,
    Color::Green,
    Color::Magenta,
    Color::Blue,
    Color::Red,
    Color::White,
    Color::LightGreen,
];

pub(crate) fn render(
    frame: &mut Frame,
    reports: &mut Reports,
    environment: String,
    terminal_focused: bool,
) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let heading = vec![
        environment,
        "Reports".into(),
        "^.: previous".into(),
        "e: edit YAML".into(),
        "r: run".into(),
        "R: reload".into(),
        "Tab: focus".into(),
        "m: metric".into(),
        "?: help".into(),
        "q: close".into(),
    ];
    frame.render_widget(
        Paragraph::new(crate::ui::styled_menu_bar_items(&heading))
            .style(Style::default().bg(crate::ui::menu_bar_background_color(terminal_focused))),
        areas[0],
    );
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(27), Constraint::Percentage(73)])
        .split(areas[1]);
    let items: Vec<_> = reports
        .entries
        .iter()
        .map(|entry| match &entry.definition {
            Ok(definition) => ListItem::new(definition.name.clone()),
            Err(_) => ListItem::new(format!(
                "! {}",
                entry.path.file_name().unwrap_or_default().to_string_lossy()
            ))
            .style(Style::default().fg(Color::Red)),
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Reports [e / r]")
        .border_style(crate::ui::active_border_style(
            reports.files_focus,
            terminal_focused,
            Color::Yellow,
        ));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(Color::Cyan)),
        panes[0],
        &mut reports.selected,
    );
    if let Some(output) = &reports.output {
        render_history(frame, reports, output, panes[1]);
    } else {
        let text = reports.selected.selected().and_then(|i| reports.entries.get(i)).map(|entry| match &entry.definition {
            Ok(definition) => format!("{}\n\n{}\n\nYAML: {}\n\nDefinitions: {}\n\n{}", definition.name, definition.description, entry.path.display(), reports.root.display(),
                if reports.running.is_some() { "Running…" } else { "r: run with inputs   e: edit this YAML in $EDITOR\nDownload or refresh ErrorLog.txt in :logs before running." }),
            Err(error) => format!("{}\n\n{error}\n\ne: repair YAML in $EDITOR", entry.path.display()),
        }).unwrap_or_else(|| format!("Add a .yaml report to {} and press R.", reports.root.display()));
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("Report")),
            panes[1],
        );
    }
    frame.render_widget(
        Paragraph::new(reports.message.clone()).wrap(Wrap { trim: false }),
        areas[2],
    );
    if let Some(form) = &reports.form {
        render_form(frame, form);
    }
}

fn render_history(frame: &mut Frame, reports: &Reports, output: &MemoryHistory, area: Rect) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let metrics = reports.metrics();
    let metric = &metrics[reports.metric % metrics.len()];
    let mut series: BTreeMap<&str, Vec<(f64, f64)>> = BTreeMap::new();
    for sample in &output.samples {
        if let Some(value) = sample.percentages.get(metric) {
            series
                .entry(&sample.host)
                .or_default()
                .push((sample.hour(), *value));
        }
    }
    let maximum = series
        .values()
        .flatten()
        .map(|(_, y)| *y)
        .fold(100.0_f64, f64::max)
        .ceil();
    let datasets = series
        .iter()
        .enumerate()
        .map(|(index, (host, data))| {
            Dataset::default()
                .name(*host)
                .marker(Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COLORS[index % COLORS.len()]))
                .data(data)
        })
        .collect();
    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "{} · {metric} % · {} (m: metric)",
            output.day, reports.result_environment
        )))
        .x_axis(
            Axis::default()
                .title("Server time")
                .bounds([0.0, 24.0])
                .labels(["00:00", "06:00", "12:00", "18:00", "24:00"]),
        )
        .y_axis(
            Axis::default()
                .title(format!("{metric} %"))
                .bounds([0.0, maximum])
                .labels([
                    "0".to_string(),
                    format!("{:.0}", maximum / 2.0),
                    format!("{maximum:.0}"),
                ]),
        );
    frame.render_widget(chart, areas[0]);
    let mut columns = vec!["used", "rss", "size"];
    if !columns.contains(&metric.as_str()) {
        columns.push(metric);
    }
    let rows = output
        .samples
        .iter()
        .skip(reports.scroll)
        .take(areas[1].height.saturating_sub(3) as usize)
        .map(|sample| {
            let mut cells = vec![
                Cell::from(sample.time.format("%H:%M:%S%.3f").to_string()),
                Cell::from(sample.host.clone()),
            ];
            cells.extend(columns.iter().map(|key| {
                Cell::from(
                    sample
                        .percentages
                        .get(*key)
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or_else(|| "—".into()),
                )
            }));
            Row::new(cells)
        });
    let mut headings = vec!["Time".to_string(), "Host".to_string()];
    headings.extend(columns.iter().map(|key| match *key {
        "used" => "Used %".into(),
        "rss" => "RSS %".into(),
        "size" => "Size %".into(),
        key => format!("{key} %"),
    }));
    let mut widths = vec![Constraint::Length(13), Constraint::Min(12)];
    widths.extend(columns.iter().map(|_| Constraint::Length(9)));
    let visible_end =
        (reports.scroll + areas[1].height.saturating_sub(3) as usize).min(output.samples.len());
    let title = format!(
        "Samples · {}–{} / {} · j/k: scroll",
        reports.scroll + 1,
        visible_end,
        output.samples.len()
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let table = Table::new(rows, widths)
        .header(Row::new(headings).style(Style::default().fg(Color::Cyan)))
        .block(block);
    frame.render_widget(table, areas[1]);
}

fn render_form(frame: &mut Frame, form: &Form) {
    let size = frame.area();
    let width = size.width.saturating_sub(4).min(80);
    let height = ((form.values.len() * 3 + 6) as u16).min(size.height);
    let area = Rect::new(
        size.x + size.width.saturating_sub(width) / 2,
        size.y + size.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (index, (input, value)) in form.definition.inputs.iter().zip(&form.values).enumerate() {
        lines.push(Line::from(input.label.clone()));
        lines.push(
            Line::from(format!(
                "{} {}",
                if index == form.selected { "›" } else { " " },
                value
            ))
            .style(Style::default().fg(if index == form.selected {
                Color::Yellow
            } else {
                Color::White
            })),
        );
        lines.push(Line::from(""));
    }
    lines.push(Line::from(
        "Tab/Shift+Tab: field · Ctrl+U: clear · Enter: run · Esc: cancel",
    ));
    lines.push(Line::from(form.error.clone()).style(Style::default().fg(Color::Red)));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Run {}", form.definition.name)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn press(app: &mut App, code: KeyCode) {
        crate::events::handle_key_event(app, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    #[test]
    fn reports_command_form_validation_help_and_escape_work_together() {
        let mut app = crate::tests::test_app();
        app.command_input = ":reports".into();
        app.execute_command();
        assert_eq!(app.mode, AppMode::Reports);
        assert_eq!(app.reports.entries.len(), 1);
        press(&mut app, KeyCode::Char('r'));
        assert!(app.reports.form.is_some());
        // Text input keeps global help/navigation shortcuts literal.
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.mode, AppMode::Reports);
        assert_eq!(app.reports.form.as_ref().unwrap().values[0], "latest?");
        press(&mut app, KeyCode::Enter);
        assert!(
            app.reports
                .form
                .as_ref()
                .unwrap()
                .error
                .contains("Choose a server")
        );
        press(&mut app, KeyCode::Esc);
        assert!(app.reports.form.is_none());
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.mode, AppMode::HelpOverlay);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, AppMode::Reports);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.mode, AppMode::StartPage);
    }

    fn sample(host: &str, time: &str, value: f64) -> reports::Sample {
        reports::Sample {
            host: host.into(),
            time: chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M:%S").unwrap(),
            percentages: BTreeMap::from([("used".into(), value), ("size".into(), 120.0)]),
        }
    }

    #[test]
    fn switching_between_logs_and_reports_preserves_both_open_dialogs() {
        let mut app = crate::tests::test_app();
        app.mode = AppMode::MarkLogicLogs;
        app.server_logs.open_truncation_dialog();
        app.server_logs.truncation_dialog.as_mut().unwrap().fields[0] =
            "2026-10-14 08:00:00".into();
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert_eq!(app.mode, AppMode::Reports);
        press(&mut app, KeyCode::Char('r'));
        app.reports.form.as_mut().unwrap().values[1] = "host-a".into();
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        assert_eq!(
            app.server_logs.truncation_dialog.as_ref().unwrap().fields[0],
            "2026-10-14 08:00:00"
        );
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert_eq!(app.mode, AppMode::Reports);
        assert_eq!(app.reports.form.as_ref().unwrap().values[1], "host-a");
    }

    #[test]
    fn reports_render_multihost_chart_samples_form_and_small_terminals() {
        let mut app = crate::tests::test_app();
        app.open_reports("");
        app.reports.output = Some(MemoryHistory {
            day: chrono::NaiveDate::from_ymd_opt(2026, 10, 26).unwrap(),
            samples: vec![
                sample("host-a", "2026-10-26 08:00:00", 35.0),
                sample("host-b", "2026-10-26 09:00:00", 65.0),
            ],
            files: 2,
            malformed: 0,
            warnings: vec![],
        });
        for (width, height) in [(140, 40), (80, 24), (30, 8), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| crate::ui::ui(frame, &mut app))
                .unwrap();
            if width == 140 {
                let buffer = terminal.backend().buffer();
                let contents: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
                assert!(contents.contains("host-a"));
                assert!(contents.contains("host-b"));
                assert!(contents.contains("Used %"));
                assert!(contents.contains("2026-10-26"));
            }
        }
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.reports.metrics()[app.reports.metric], "size");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.reports.scroll, 1);
        press(&mut app, KeyCode::Char('r'));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let contents: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(contents.contains("Day (YYYY-MM-DD, or latest)"));
        assert!(contents.contains("Host (blank for all hosts)"));
    }

    #[test]
    fn report_runs_in_background_and_applies_its_completion() {
        let mut app = crate::tests::test_app();
        app.open_reports("");
        let root = app.query_root_dir.join("report-cache");
        std::fs::create_dir_all(&root).unwrap();
        let text =
            "2026-10-26 08:00:00.123 Info: Memory 35% phys=1000 size=1200(120%) rss=350(35%)\n";
        std::fs::write(root.join("host.json"), serde_json::to_vec(&serde_json::json!({
            "file": { "host": "host-a", "name": "ErrorLog.txt", "path": "/Logs/ErrorLog.txt", "size": text.len(), "modified": "" },
            "start": 0, "end": text.len(), "length": text.len(), "anchor_start": 0, "anchor": "", "text": text, "generation": 0
        })).unwrap()).unwrap();
        app.reports.cache = Some(root);
        press(&mut app, KeyCode::Char('r'));
        press(&mut app, KeyCode::Enter);
        assert!(app.reports.running.is_some());
        assert!(app.reports.form.is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while app.reports.running.is_some() && std::time::Instant::now() < deadline {
            app.reports.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.reports.running.is_none(), "worker timed out");
        let output = app.reports.output.as_ref().expect(&app.reports.message);
        assert_eq!(output.day.to_string(), "2026-10-26");
        assert_eq!(output.samples[0].host, "host-a");
        assert_eq!(output.samples[0].percentages["used"], 35.0);
        assert!(
            app.reports
                .message
                .contains("1 samples from 1 cached files")
        );
        std::fs::remove_dir_all(app.query_root_dir).unwrap();
    }

    #[test]
    fn switching_report_source_discards_old_completion_and_output() {
        let mut app = crate::tests::test_app();
        app.open_reports("");
        let (sender, receiver) = mpsc::channel();
        app.reports.running = Some(receiver);
        app.reports.cache = Some(PathBuf::from("old-server-cache"));
        app.reconcile_report_source();
        assert!(app.reports.running.is_none());
        assert!(sender.send(Err(anyhow::anyhow!("stale result"))).is_err());
        assert!(app.reports.output.is_none());
    }
}
