//! Local TUI diagnostics and preferences, independent of a MarkLogic connection.
use crate::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use tracing_subscriber::{Registry, filter::LevelFilter, reload};

pub(crate) type LogFilterHandle = reload::Handle<LevelFilter, Registry>;
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const MAX_LOG_LINES: usize = 10_000;
const SEARCH_PROMPT: &str = "Filter (Enter/Esc finish): ";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
    #[default]
    Debug,
    Trace,
}

impl LogLevel {
    const ALL: [Self; 5] = [
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];

    pub fn filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Debug => LevelFilter::DEBUG,
            Self::Trace => LevelFilter::TRACE,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InlineEditor {
    #[default]
    Edtui,
    Textarea,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub log_level: LogLevel,
    pub inline_editor: InlineEditor,
    #[serde(deserialize_with = "deserialize_autosave")]
    pub autosave_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Debug,
            inline_editor: InlineEditor::Edtui,
            autosave_seconds: 2,
        }
    }
}

fn deserialize_autosave<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let seconds = u64::deserialize(d)?;
    if (1..=60).contains(&seconds) {
        Ok(seconds)
    } else {
        Err(serde::de::Error::custom(
            "autosave_seconds must be between 1 and 60",
        ))
    }
}

pub(crate) struct System {
    pub settings_tab: bool,
    pub selected_setting: usize,
    pub follow: bool,
    pub search: String,
    pub search_editing: bool,
    pub return_mode: Option<AppMode>,
    pub return_screen: Option<Screen>,
    pub log_filter: Option<LogFilterHandle>,
    started: Instant,
    checked: Option<Instant>,
    fingerprint: Option<(u64, Option<SystemTime>)>,
    raw_log: String,
    message: String,
    truncated: bool,
    horizontal: u16,
}

impl Default for System {
    fn default() -> Self {
        Self {
            settings_tab: false,
            selected_setting: 0,
            follow: true,
            search: String::new(),
            search_editing: false,
            return_mode: None,
            return_screen: None,
            log_filter: None,
            started: Instant::now(),
            checked: None,
            fingerprint: None,
            raw_log: String::new(),
            message: String::new(),
            truncated: false,
            horizontal: 0,
        }
    }
}

impl System {
    pub fn invalidate_log(&mut self) {
        self.checked = None;
        self.fingerprint = None;
    }

    /// Bound disk reads and memory; unchanged files are not reread on each draw.
    pub fn refresh_log(&mut self, path: &Path) -> bool {
        if self
            .checked
            .is_some_and(|last| last.elapsed() < Duration::from_millis(500))
        {
            return false;
        }
        self.checked = Some(Instant::now());
        let result = (|| -> io::Result<bool> {
            let mut file = fs::File::open(path)?;
            let metadata = file.metadata()?;
            let fingerprint = (metadata.len(), metadata.modified().ok());
            if self.fingerprint == Some(fingerprint) {
                return Ok(false);
            }
            let start = metadata.len().saturating_sub(MAX_LOG_BYTES);
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::new();
            file.take(MAX_LOG_BYTES).read_to_end(&mut bytes)?;
            // A tail can start inside a UTF-8 character or a record. Skip that record.
            let bytes = if start > 0 {
                bytes
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(&[][..], |i| &bytes[i + 1..])
            } else {
                &bytes
            };
            let text = String::from_utf8_lossy(bytes);
            let skip = text.lines().count().saturating_sub(MAX_LOG_LINES);
            self.raw_log = text.lines().skip(skip).collect::<Vec<_>>().join("\n");
            self.truncated = start > 0 || skip > 0;
            self.fingerprint = Some(fingerprint);
            Ok(true)
        })();
        match result {
            Ok(changed) => changed,
            Err(error) => {
                self.raw_log = format!("Cannot read diagnostic log {}: {error}", path.display());
                self.fingerprint = None;
                self.truncated = false;
                true
            }
        }
    }

    pub fn filtered_log(&self) -> String {
        let needle = self.search.to_lowercase();
        self.raw_log
            .lines()
            .filter(|line| needle.is_empty() || line.to_lowercase().contains(&needle))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl App {
    pub fn handle_system_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.system.search_editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.system.search_editing = false,
                KeyCode::Backspace => {
                    self.system.search.pop();
                }
                KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                    self.system.search.clear()
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.system.search.push(ch)
                }
                _ => {}
            }
            self.log_view_content = self.system.filtered_log();
            self.log_view_scroll = 0;
            return Ok(false);
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => self.system.settings_tab = !self.system.settings_tab,
            KeyCode::Char('q') => self.close_log_viewer(),
            KeyCode::Char(':') => {
                self.enter_start_page();
                self.command_input = ":".into();
                self.update_autocomplete();
            }
            KeyCode::Esc if self.system.settings_tab => {
                self.system.selected_setting = 0;
                self.system.message.clear();
            }
            KeyCode::Down | KeyCode::Char('j') if self.system.settings_tab => {
                self.system.selected_setting = (self.system.selected_setting + 1) % 3;
            }
            KeyCode::Up | KeyCode::Char('k') if self.system.settings_tab => {
                self.system.selected_setting = (self.system.selected_setting + 2) % 3;
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                if self.system.settings_tab =>
            {
                self.change_system_setting(key.code == KeyCode::Left);
            }
            KeyCode::Char('/') if !self.system.settings_tab => {
                self.system.search_editing = true;
                self.system.follow = false;
            }
            KeyCode::Char('c') if !self.system.settings_tab => {
                self.system.search.clear();
                self.log_view_content = self.system.filtered_log();
                self.log_view_scroll = 0;
            }
            KeyCode::Char('f') if !self.system.settings_tab => {
                self.system.follow = !self.system.follow
            }
            KeyCode::Left | KeyCode::Char('h') if !self.system.settings_tab => {
                self.system.horizontal = self.system.horizontal.saturating_sub(8);
            }
            KeyCode::Right | KeyCode::Char('l') if !self.system.settings_tab => {
                self.system.horizontal = self.system.horizontal.saturating_add(8);
            }
            KeyCode::Esc if !self.system.settings_tab => {
                self.system.horizontal = 0;
                self.system.follow = false;
                self.log_view_scroll = 0;
            }
            KeyCode::Char('r') if !self.system.settings_tab => self.system.invalidate_log(),
            KeyCode::Char('e') if !self.system.settings_tab => {
                self.open_log_in_external_editor();
                self.system.message = self.status_message.clone();
            }
            KeyCode::Char('G') if !self.system.settings_tab => {
                self.log_view_scroll = self.log_viewer_max_scroll();
                self.system.follow = true;
            }
            _ if !self.system.settings_tab => {
                return crate::events::handle_log_viewer_key(self, key);
            }
            _ => {}
        }
        Ok(false)
    }

    fn change_system_setting(&mut self, backwards: bool) {
        let mut settings = self.config.system.clone();
        match self.system.selected_setting {
            0 => {
                let index = LogLevel::ALL
                    .iter()
                    .position(|level| *level == settings.log_level)
                    .unwrap_or(3);
                settings.log_level = LogLevel::ALL[(index + if backwards { 4 } else { 1 }) % 5];
                if let Some(handle) = &self.system.log_filter {
                    if let Err(error) = handle.reload(settings.log_level.filter()) {
                        self.system.message = format!("Cannot change diagnostic level: {error}");
                        return;
                    }
                }
            }
            1 => {
                // Capture the active buffer before changing editor implementations.
                let contents = self.query_text();
                self.use_edtui = !self.use_edtui;
                settings.inline_editor = if self.use_edtui {
                    InlineEditor::Edtui
                } else {
                    InlineEditor::Textarea
                };
                self.query_editor = Self::new_query_editor(editor_lines(&contents));
                self.edtui_state = EditorState::new(Lines::from(contents.as_str()));
                self.edtui_handler = EditorEventHandler::default();
                self.set_edit_mode(EditMode::Navigate);
                // Do not restore a stale insert/visual mode when returning to Query.
                if let Some(snapshot) = self.screen_states.get_mut(&Screen::Query) {
                    snapshot.edit_mode = EditMode::Navigate;
                    snapshot.editor_mode = EditorMode::Normal;
                }
            }
            _ => {
                const INTERVALS: [u64; 5] = [1, 2, 5, 10, 30];
                settings.autosave_seconds = if backwards {
                    INTERVALS
                        .iter()
                        .rev()
                        .copied()
                        .find(|seconds| *seconds < settings.autosave_seconds)
                        .unwrap_or(30)
                } else {
                    INTERVALS
                        .iter()
                        .copied()
                        .find(|seconds| *seconds > settings.autosave_seconds)
                        .unwrap_or(1)
                };
                self.query_autosave_interval = Duration::from_secs(settings.autosave_seconds);
            }
        }
        self.config.system = settings;
        self.system.message = match self.save_app_config() {
            Ok(()) => "Setting applied and saved.".into(),
            Err(error) => format!("Setting applied; could not save all configuration: {error:#}"),
        };
        info!(setting = self.system.selected_setting, settings = ?self.config.system, "System preference changed");
    }
}

pub(crate) fn render(f: &mut Frame, app: &mut App) {
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(f.area());
    f.render_widget(
        Paragraph::new("System · TUI diagnostics and preferences ")
            .alignment(Alignment::Right)
            .style(
                Style::default()
                    .bg(crate::ui::menu_bar_background_color(app.terminal_focused))
                    .fg(crate::ui::menu_bar_text_color()),
            ),
        areas[0],
    );
    let elapsed = app.system.started.elapsed().as_secs();
    f.render_widget(Paragraph::new(format!(
        " marklogic-tui v{} · PID {} · uptime {}m {:02}s · {} running queries\n Local application activity and preferences",
        env!("CARGO_PKG_VERSION"), std::process::id(), elapsed / 60, elapsed % 60, app.running_queries.len()
    )).style(Style::default().fg(Color::Gray)), areas[1]);
    let tabs = [" Logs ", " Settings "]
        .into_iter()
        .enumerate()
        .map(|(i, label)| {
            Span::styled(
                label,
                if (i == 1) == app.system.settings_tab {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            )
        })
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(Line::from(tabs)), areas[2]);
    app.mouse_regions.push((
        Rect::new(
            areas[2].x,
            areas[2].y,
            areas[2].width.min(6),
            areas[2].height,
        ),
        MouseTarget::SystemLogs,
    ));
    if areas[2].width > 6 {
        app.mouse_regions.push((
            Rect::new(
                areas[2].x + 6,
                areas[2].y,
                (areas[2].width - 6).min(10),
                areas[2].height,
            ),
            MouseTarget::SystemSettings,
        ));
    }
    if app.system.settings_tab {
        render_settings(f, app, areas[3]);
    } else {
        app.refresh_log_viewer_content();
        app.log_viewer_area_height = areas[3].height;
        app.log_view_scroll = if app.system.follow {
            app.log_viewer_max_scroll()
        } else {
            app.log_view_scroll.min(app.log_viewer_max_scroll())
        };
        let lines: Vec<Line> = if app.log_view_content.is_empty() {
            vec![Line::from(if app.system.search.is_empty() {
                "No diagnostic entries yet."
            } else {
                "No entries match this filter."
            })]
        } else {
            app.log_view_content
                .lines()
                .skip(usize::from(app.log_view_scroll))
                .take(usize::from(areas[3].height.saturating_sub(2)))
                .map(|line| {
                    let color = if line.contains(" ERROR ") {
                        Color::Red
                    } else if line.contains(" WARN ") {
                        Color::Yellow
                    } else if line.contains(" DEBUG ") || line.contains(" TRACE ") {
                        Color::DarkGray
                    } else {
                        Color::White
                    };
                    Line::styled(line, Style::default().fg(color))
                })
                .collect()
        };
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    " Application log · {} · {} · {} lines{} ",
                    app.log_file_path.display(),
                    if app.system.follow {
                        "FOLLOW"
                    } else {
                        "SCROLL"
                    },
                    app.log_view_content.lines().count(),
                    if app.system.truncated {
                        " · recent tail only"
                    } else {
                        ""
                    }
                )))
                .scroll((0, app.system.horizontal)),
            areas[3],
        );
    }
    let footer = if app.system.settings_tab {
        format!(
            " Tab: Logs · j/k: select · ←/→/Enter: change & save · q: close · ?: help\n {}",
            app.system.message
        )
    } else {
        format!(
            " Tab: Settings · /: filter · c: clear · f: follow · j/k: scroll · e: editor · q: close\n {}{}{}",
            if app.system.search_editing {
                SEARCH_PROMPT
            } else {
                "Filter: "
            },
            if app.system.search.is_empty() && !app.system.search_editing {
                "(none)"
            } else {
                &app.system.search
            },
            if app.system.message.is_empty() {
                String::new()
            } else {
                format!(" · {}", app.system.message)
            }
        )
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::Cyan)),
        areas[4],
    );
    if app.system.search_editing && areas[4].height > 1 && areas[4].width > 0 {
        let x = (1
            + SEARCH_PROMPT.len()
            + unicode_width::UnicodeWidthStr::width(app.system.search.as_str()))
        .min(usize::from(areas[4].width - 1)) as u16;
        f.set_cursor_position((areas[4].x + x.min(areas[4].width - 1), areas[4].y + 1));
    }
}

fn render_settings(f: &mut Frame, app: &mut App, area: Rect) {
    let areas = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).split(area);
    let editor = if app.use_edtui {
        "edtui (Vim + highlighting)"
    } else {
        "textarea (simple)"
    };
    let items = vec![
        ListItem::new(format!(
            "Diagnostic log level    {:?}",
            app.config.system.log_level
        )),
        ListItem::new(format!("Inline query editor    {editor}")),
        ListItem::new(format!(
            "Query autosave delay   {} seconds",
            app.query_autosave_interval.as_secs()
        )),
    ];
    let mut selection = ListState::default().with_selected(Some(app.system.selected_setting));
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Preferences · saved immediately "),
            )
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        areas[0],
        &mut selection,
    );
    for i in 0..3 {
        if areas[0].height > i + 2 {
            app.mouse_regions.push((
                Rect::new(areas[0].x, areas[0].y + 1 + i, areas[0].width, 1),
                MouseTarget::SystemSetting(i as usize),
            ));
        }
    }
    let description = match app.system.selected_setting {
        0 => {
            "Controls which new entries are written to the diagnostic log. Changes take effect immediately; existing entries are retained."
        }
        1 => {
            "Changes the inline editor immediately, preserving the query buffer. --inline-editor overrides the saved preference on launch."
        }
        _ => {
            "Wait after the last query edit before saving automatically. Ctrl+S always saves immediately. Choose 1, 2, 5, 10 or 30 seconds."
        }
    };
    let text = format!(
        "{description}\n\nConfiguration: {}\nConfig bundle: {}\nDiagnostic log: {}\nQuery folder: {}\nQuery result cache: {}\nExternal editor: {}",
        AppConfig::config_path().display(),
        app.config_bundle_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into()),
        app.log_file_path.display(),
        app.query_root_dir.display(),
        app.query_result_cache_dir.display(),
        env::var("EDITOR").unwrap_or_else(|_| "(not set)".into())
    );
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Gray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" About this setting and local paths "),
            ),
        areas[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_app;
    use ratatui::{Terminal, backend::TestBackend};

    fn press(app: &mut App, code: KeyCode) {
        crate::events::handle_key_event(app, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn system_settings_support_old_config_and_validate_saved_values() {
        let old: AppConfig = toml::from_str("servers = []").unwrap();
        assert_eq!(old.system, Settings::default());
        let partial: AppConfig = toml::from_str("[system]\nlog_level = 'warn'").unwrap();
        assert_eq!(partial.system.log_level, LogLevel::Warn);
        assert_eq!(partial.system.autosave_seconds, 2);
        for bad in ["0", "61", "-1", "'two'"] {
            assert!(
                toml::from_str::<AppConfig>(&format!("[system]\nautosave_seconds = {bad}"))
                    .is_err()
            );
        }
        let settings = Settings {
            log_level: LogLevel::Info,
            inline_editor: InlineEditor::Textarea,
            autosave_seconds: 10,
        };
        let yaml = serde_yaml::to_string(&settings).unwrap();
        assert_eq!(serde_yaml::from_str::<Settings>(&yaml).unwrap(), settings);
    }

    #[test]
    fn system_shortcuts_and_alias_open_without_a_connection_and_restore_screen() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus = Focus::Command;
        app.command_input = ":system".into();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current_screen(), Screen::System);
        press(&mut app, KeyCode::Tab);
        assert!(app.system.settings_tab);
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.mode, AppMode::HelpOverlay);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.mode, AppMode::StartPage);

        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Char('6'), modifiers))
                .unwrap();
            assert_eq!(app.mode, AppMode::System);
            assert!(app.system.settings_tab);
            crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Char('.'), modifiers))
                .unwrap();
            assert_eq!(app.mode, AppMode::StartPage);
        }
        press(&mut app, KeyCode::F(2));
        assert_eq!(app.mode, AppMode::System);
        assert!(!app.system.settings_tab);
    }

    #[test]
    fn system_settings_preserve_unsaved_query_and_persist_to_config_and_bundle() {
        let mut app = test_app();
        app.query_visible = true;
        app.query_editor =
            App::new_query_editor(vec!["let $a := 'unsaved'".into(), "return $a".into()]);
        app.query_dirty = true;
        let original = app.query_text();
        app.config_bundle_path = Some(app.query_root_dir.join("bundle.yaml"));
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('6'), KeyModifiers::ALT),
        )
        .unwrap();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        assert!(app.use_edtui);
        assert_eq!(app.query_text(), original);
        assert!(app.query_dirty);
        press(&mut app, KeyCode::Enter);
        assert!(!app.use_edtui);
        assert_eq!(app.query_text(), original);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.query_autosave_interval, Duration::from_secs(5));
        let saved = AppConfig::load().unwrap();
        assert_eq!(saved.system, app.config.system);
        let bundle: ConfigBundle = serde_yaml::from_str(
            &fs::read_to_string(app.config_bundle_path.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(bundle.config.system, app.config.system);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.current_screen(), Screen::Query);
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert_eq!(app.query_text(), original);
    }

    #[test]
    fn system_log_level_reload_changes_enabled_events_immediately() {
        use tracing_subscriber::prelude::*;
        let (filter, handle) = reload::Layer::new(LevelFilter::DEBUG);
        let subscriber = tracing_subscriber::registry().with(filter);
        tracing::subscriber::with_default(subscriber, || {
            let mut app = test_app();
            app.system.log_filter = Some(handle);
            assert!(tracing::enabled!(tracing::Level::DEBUG));
            app.change_system_setting(false); // Debug -> Trace
            assert!(tracing::enabled!(tracing::Level::TRACE));
            app.change_system_setting(false); // Trace -> Error
            assert!(!tracing::enabled!(tracing::Level::WARN));
            assert!(tracing::enabled!(tracing::Level::ERROR));
            assert_eq!(AppConfig::load().unwrap().system.log_level, LogLevel::Error);
        });
    }

    #[test]
    fn system_log_tail_is_bounded_and_recovers_after_replacement_or_missing_file() {
        let app = test_app();
        let path = app.query_root_dir.join("diagnostics.log");
        let mut system = System::default();
        assert!(system.refresh_log(&path));
        assert!(system.filtered_log().contains("Cannot read"));
        let content = (0..20_000)
            .map(|i| format!("{i}: {}\n", "é".repeat(60)))
            .collect::<String>();
        fs::write(&path, content).unwrap();
        system.invalidate_log();
        assert!(system.refresh_log(&path));
        assert!(system.truncated);
        assert!(system.raw_log.len() <= MAX_LOG_BYTES as usize);
        assert!(system.raw_log.lines().count() <= MAX_LOG_LINES);
        assert!(
            system
                .raw_log
                .ends_with(&format!("19999: {}", "é".repeat(60)))
        );
        system.checked = None;
        assert!(
            !system.refresh_log(&path),
            "unchanged log should not be reread"
        );
        fs::write(&path, "INFO replaced\nERROR New failure\n").unwrap();
        system.checked = None;
        assert!(system.refresh_log(&path));
        assert!(!system.truncated);
        system.search = "FAILURE".into();
        assert_eq!(system.filtered_log(), "ERROR New failure");
    }

    #[test]
    fn system_filter_accepts_navigation_and_help_characters_as_text() {
        let mut app = test_app();
        app.open_system();
        press(&mut app, KeyCode::Char('/'));
        for ch in ['q', '?', ':'] {
            press(&mut app, KeyCode::Char(ch));
        }
        assert_eq!(app.mode, AppMode::System);
        assert_eq!(app.system.search, "q?:");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('c'));
        assert!(app.system.search.is_empty());
        assert!(!app.system.follow);
    }

    #[test]
    fn system_render_follows_new_logs_and_keeps_manual_scroll_position() {
        let mut app = test_app();
        app.log_file_path = app.query_root_dir.join("activity.log");
        let mut content = (0..40)
            .map(|i| format!(" INFO activity {i}\n"))
            .collect::<String>();
        fs::write(&app.log_file_path, &content).unwrap();
        app.open_system();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        assert!(screen_text(&terminal).contains("activity 39"));
        assert!(app.log_view_scroll > 0);
        press(&mut app, KeyCode::Up);
        let scroll = app.log_view_scroll;
        content.push_str(" ERROR newest activity\n");
        fs::write(&app.log_file_path, &content).unwrap();
        app.system.invalidate_log();
        terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        assert_eq!(app.log_view_scroll, scroll);
        press(&mut app, KeyCode::Char('G'));
        terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        assert!(screen_text(&terminal).contains("newest activity"));
        press(&mut app, KeyCode::Tab);
        terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Diagnostic log level"));
        assert!(text.contains("Configuration:"));
        for (width, height) in [(40, 10), (12, 4), (1, 1)] {
            let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
            small.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
        }
    }
}
