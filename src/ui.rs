use super::*;
use crate::query_file::file_list_width;
use std::time::UNIX_EPOCH;

/// Format a DocumentDetail for the full-screen view with metadata header + content
/// Create a single-line snippet from a result string, truncated to max_len.
/// Collapses lines without materializing the whole collapsed string, so the
/// cost stays proportional to `max_len` rather than to the result size.
fn make_snippet(text: &str, max_len: usize) -> String {
    let mut snippet = String::new();
    let mut started = false;
    let mut truncated = false;
    let mut chars = 0usize;
    'outer: for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if started {
            if chars >= max_len {
                truncated = true;
                break;
            }
            snippet.push(' ');
            chars += 1;
        }
        started = true;
        for ch in trimmed.chars() {
            if chars >= max_len {
                truncated = true;
                break 'outer;
            }
            snippet.push(ch);
            chars += 1;
        }
    }
    if truncated {
        snippet.push_str("...");
    }
    snippet
}

/// Split a delimited row without losing delimiters embedded in a quoted field.
/// This intentionally accepts imperfect input: query output is often produced
/// by ad-hoc scripts, and showing the best available table is more useful than
/// rejecting a whole result set over one malformed quote.
fn parse_delimited_row(row: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = row.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            _ if ch == delimiter && !quoted => {
                fields.push(field.trim().to_string());
                field.clear();
            }
            _ => field.push(ch),
        }
    }
    fields.push(field.trim().to_string());
    fields
}

fn parse_delimited_results(results: &[String], delimiter: char) -> Vec<Vec<String>> {
    results
        .iter()
        .flat_map(|result| result.lines())
        .filter(|line| !line.is_empty())
        .map(|line| parse_delimited_row(line, delimiter))
        .collect()
}

fn pretty_json_results(results: &[String]) -> String {
    let values: Result<Vec<serde_json::Value>, _> = results
        .iter()
        .map(|result| serde_json::from_str(result.trim()))
        .collect();
    match values {
        Ok(values) if values.len() == 1 => {
            serde_json::to_string_pretty(&values[0]).unwrap_or_else(|_| results[0].clone())
        }
        Ok(values) => serde_json::to_string_pretty(&values).unwrap_or_else(|_| results.join("\n")),
        Err(_) => results
            .iter()
            .map(|result| {
                serde_json::from_str::<serde_json::Value>(result.trim())
                    .ok()
                    .and_then(|value| serde_json::to_string_pretty(&value).ok())
                    .unwrap_or_else(|| result.clone())
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

pub(crate) fn format_document_detail(detail: &client::DocumentDetail) -> String {
    let mut output = String::new();
    output.push_str("━━━ Metadata ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    output.push_str(&format!("URI:          {}\n", detail.uri));
    output.push_str(&format!(
        "Collections:  {}\n",
        if detail.collections.is_empty() {
            "(none)".to_string()
        } else {
            detail.collections.join(", ")
        }
    ));
    if let Some(q) = detail.quality {
        output.push_str(&format!("Quality:      {}\n", q));
    }
    if !detail.permissions.is_empty() {
        output.push_str(&format!(
            "Permissions:  {}\n",
            detail.permissions.join(", ")
        ));
    }
    output.push_str("━━━ Content ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n");
    output.push_str(&format_document_content(&detail.content));
    output
}

/// Format document content based on type
pub(crate) fn format_document_content(text: &str) -> String {
    let trimmed = text.trim();
    // Try JSON
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return serde_json::to_string_pretty(&val).unwrap_or_else(|_| text.to_string());
    }
    // Try XML - do basic indentation
    if trimmed.starts_with('<') {
        return format_xml(trimmed);
    }
    // Fallback: plain text
    text.to_string()
}

/// Basic XML indentation formatter
fn format_xml(xml: &str) -> String {
    let mut result = String::new();
    let mut indent: usize = 0;
    let mut i = 0;
    let bytes = xml.as_bytes();

    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find end of tag
            let tag_start = i;
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // include '>'
            }
            let tag = &xml[tag_start..i];

            if tag.starts_with("</") {
                // Closing tag - decrease indent
                indent = indent.saturating_sub(1);
                result.push_str(&"  ".repeat(indent));
                result.push_str(tag);
                result.push('\n');
            } else if tag.ends_with("/>") || tag.starts_with("<?") || tag.starts_with("<!") {
                // Self-closing or processing instruction
                result.push_str(&"  ".repeat(indent));
                result.push_str(tag);
                result.push('\n');
            } else {
                // Opening tag
                result.push_str(&"  ".repeat(indent));
                result.push_str(tag);
                result.push('\n');
                indent += 1;
            }
        } else {
            // Text content
            let text_start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            let text_content = xml[text_start..i].trim();
            if !text_content.is_empty() {
                result.push_str(&"  ".repeat(indent));
                result.push_str(text_content);
                result.push('\n');
            }
        }
    }
    result
}

pub(crate) fn ui(f: &mut Frame, app: &mut App) {
    app.mouse_regions.clear();
    app.environment_area = Rect::default();
    app.server_picker_area = Rect::default();
    f.render_widget(
        Block::default().style(Style::default().bg(app_background_color())),
        f.area(),
    );

    match app.mode {
        AppMode::StartPage => ui_start_page(f, app),
        AppMode::FullScreenView => ui_fullscreen(f, app),
        AppMode::System => crate::system::render(f, app),
        AppMode::MarkLogicLogs => {
            app.server_logs.environment_indicator = app.environment_indicator();
            logs_ui::render(f, &mut app.server_logs, app.terminal_focused)
        }
        AppMode::Reports => {
            let environment = app.environment_indicator();
            crate::reports_ui::render(f, &mut app.reports, environment, app.terminal_focused);
        }
        AppMode::Interface(AppInterface::Servers) => ui_servers_interface(f, app),
        AppMode::ServerForm => ui_server_form(f, app),
        AppMode::ServerDeleteConfirm => ui_server_delete_confirm(f, app),
        AppMode::AppServerForm => ui_app_server_form(f, app),
        AppMode::AppServerDeleteConfirm => ui_app_server_delete_confirm(f, app),
        AppMode::AppServerDiscovery => ui_app_server_discovery(f, app),
        AppMode::CollectionSelect => ui_collection_select(f, app),
        AppMode::DeleteConfirm => ui_delete_confirm(f, app),
        AppMode::QueryFileSelect => ui_query_file_select(f, app),
        AppMode::QueryFileCreate => ui_query_file_create(f, app),
        AppMode::QueryFileRename => ui_query_file_rename(f, app),
        AppMode::QueryFileDeleteConfirm => ui_query_file_delete_confirm(f, app),
        AppMode::QueryResultFormatSelect => ui_query_result_format_select(f, app),
        AppMode::ActiveVariables => ui_active_variables(f, app),
        AppMode::DeclaredVariables => ui_declared_variables(f, app),
        AppMode::MultiRunForm => ui_multi_run_form(f, app),
        AppMode::ModuleCloneSelect => ui_module_clone_select(f, app),
        AppMode::ModuleResetConfirm => ui_module_reset_confirm(f, app),
        AppMode::ModuleDropConfirm => ui_module_drop_confirm(f, app),
        AppMode::ModuleDiffView => ui_module_diff_view(f, app),
        AppMode::ModuleDropCacheConfirm => ui_module_drop_cache_confirm(f, app),
        AppMode::ModuleCreate => ui_module_create(f, app),
        AppMode::TrackedFolderSelect => ui_tracked_folder_select(f, app),
        AppMode::TrackedFolderAdd => ui_tracked_folder_add(f, app),
        AppMode::TrackedFolderDeleteConfirm => ui_tracked_folder_delete_confirm(f, app),
        AppMode::TrackedFolderCacheClearConfirm => ui_tracked_folder_cache_clear_confirm(f, app),
        AppMode::HelpOverlay => ui_help(f, app),
        AppMode::Normal => ui_normal(f, app),
        AppMode::DocumentCreate => ui_document_create(f, app),
        AppMode::DocumentMetadataEdit => ui_document_metadata_edit(f, app),
    }
    if app.mode != AppMode::StartPage {
        let area = f.area();
        let indicator = app.environment_indicator();
        let width =
            (unicode_width::UnicodeWidthStr::width(indicator.as_str()) as u16).min(area.width);
        app.environment_area = Rect::new(area.x, area.y, width, area.height.min(1));
        f.render_widget(
            Paragraph::new(indicator).style(
                Style::default()
                    .fg(menu_bar_text_color())
                    .bg(menu_bar_background_color(app.terminal_focused))
                    .add_modifier(Modifier::BOLD),
            ),
            app.environment_area,
        );
        if app.server_picker_open && area.height > 1 && area.width > 0 {
            let height = (app.config.servers.len().max(1) as u16)
                .saturating_add(2)
                .min(area.height - 1);
            let width = area.width.min(45);
            let picker = Rect::new(area.x, area.y + 1, width, height);
            app.server_picker_area = picker;
            let items: Vec<ListItem> = if app.config.servers.is_empty() {
                vec![ListItem::new("No servers configured")]
            } else {
                app.config
                    .servers
                    .iter()
                    .map(|server| {
                        ListItem::new(format!(
                            "{}:{}{}",
                            server.name,
                            server.port,
                            if app.config.active_server.as_deref() == Some(&server.name) {
                                " (active)"
                            } else {
                                ""
                            }
                        ))
                    })
                    .collect()
            };
            f.render_widget(Clear, picker);
            let mut block = Block::default()
                .borders(Borders::ALL)
                .title("Choose server");
            if let Some(error) = &app.server_picker_error {
                block = block
                    .title_bottom(Line::from(error.clone()).style(Style::default().fg(Color::Red)));
            }
            let list = List::new(items)
                .block(block)
                .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
                .highlight_symbol("› ");
            f.render_stateful_widget(list, picker, &mut app.server_picker_state);
        }
    }
}

fn app_background_color() -> Color {
    Color::Rgb(6, 12, 28)
}

pub(crate) fn menu_bar_background_color(terminal_focused: bool) -> Color {
    if terminal_focused {
        Color::Rgb(160, 150, 110)
    } else {
        Color::DarkGray
    }
}

pub(crate) fn active_border_style(
    selected: bool,
    terminal_focused: bool,
    focused_color: Color,
) -> Style {
    Style::default().fg(if selected && terminal_focused {
        focused_color
    } else {
        Color::DarkGray
    })
}

fn panel_border_style(selected: bool, terminal_focused: bool) -> Style {
    active_border_style(selected, terminal_focused, Color::Yellow)
}

pub(crate) fn menu_bar_text_color() -> Color {
    Color::Rgb(6, 12, 28)
}

fn panel_block<T>(title: T, shortcut: &'static str) -> Block<'static>
where
    T: Into<Line<'static>>,
{
    let title_line: Line<'static> = Line::from(vec![
        Span::raw(shortcut),
        Span::raw("─ "),
        title.into().spans.into_iter().next().unwrap_or_default(),
    ]);
    Block::default().borders(Borders::ALL).title(title_line)
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Right-aligned red "running Ns" title shown where the result age
/// normally appears while a query is executing.
fn running_title(elapsed: Duration) -> Line<'static> {
    Line::from(Span::styled(
        format!("running {}", format_elapsed(elapsed)),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Right)
}

fn format_relative_time(timestamp: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(timestamp)
        .unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 604800 {
        format!("{}d ago", secs / 86400)
    } else {
        format!("{}w ago", secs / 604800)
    }
}

pub(crate) fn display_folder_path(path: &Path) -> String {
    path.display().to_string()
}

pub(crate) fn resolve_folder_input(current_root: &Path, input: &str) -> PathBuf {
    if let Some(home_dir) = dirs::home_dir() {
        if input == "~" {
            return home_dir;
        }
        if let Some(stripped) = input.strip_prefix("~/") {
            return home_dir.join(stripped);
        }
    }

    let input_path = Path::new(input);
    if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        current_root.join(input_path)
    }
}

fn command_indent_level(cmd: &str) -> usize {
    match cmd {
        ":server-add" | ":databases" => 1,
        ":collections" | ":list:<collection>" | ":clear" => 1,
        ":query-files" | ":query-open" | ":folders" => 1,
        _ => 0,
    }
}

fn keybinding_items(
    focus: &Focus,
    edit_mode: &EditMode,
    file_list_visible: bool,
    is_synced_module: bool,
    modules_view: bool,
) -> Vec<(String, &'static str)> {
    crate::help::bar_items(
        focus,
        edit_mode,
        file_list_visible,
        is_synced_module,
        modules_view,
    )
    .into_iter()
    .map(|(key, label)| (crate::help::bar_key(key), label))
    .collect()
}

fn keybindings_line(
    focus: &Focus,
    edit_mode: &EditMode,
    file_list_visible: bool,
    is_synced_module: bool,
    modules_view: bool,
) -> Line<'static> {
    let items = keybinding_items(
        focus,
        edit_mode,
        file_list_visible,
        is_synced_module,
        modules_view,
    );
    let key_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Rgb(60, 60, 60));
    let divider_style = Style::default().fg(Color::Rgb(100, 100, 100));

    let mut spans = Vec::new();
    for (i, (key, label)) in items.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", divider_style));
        }
        spans.push(Span::styled(key, key_style));
        spans.push(Span::styled(format!(" {}", label), label_style));
    }

    Line::from(spans)
}

pub(crate) fn styled_menu_bar_items(items: &[String]) -> Line<'static> {
    let white_key = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::Rgb(60, 60, 60));
    let sep_style = Style::default().fg(Color::Rgb(100, 100, 100));

    let mut spans = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", sep_style));
        }

        let item = item.trim();
        if let Some((keys_part, desc)) = item.split_once(": ") {
            let keys: Vec<&str> = keys_part.split('/').collect();
            for (j, key) in keys.iter().enumerate() {
                if j > 0 {
                    spans.push(Span::styled("/", sep_style));
                }
                spans.push(Span::styled(key.to_string(), white_key));
            }
            spans.push(Span::styled(format!(": {}", desc), desc_style));
        } else if let Some(pos) = item.find(' ') {
            let (key, desc) = item.split_at(pos);
            spans.push(Span::styled(key.to_string(), white_key));
            spans.push(Span::styled(desc.to_string(), desc_style));
        } else {
            spans.push(Span::styled(item.to_string(), white_key));
        }
    }
    Line::from(spans)
}

fn ui_start_page(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let mut prompt_width = area.width.saturating_sub(8);
    prompt_width = prompt_width.min(72);
    if prompt_width < 20 {
        prompt_width = area.width.saturating_sub(2);
    }
    if prompt_width == 0 {
        prompt_width = area.width.max(1);
    }

    let prompt_height = 3;

    let prompt_area = Rect {
        x: area.x + area.width.saturating_sub(prompt_width) / 2,
        y: area.y + area.height.saturating_sub(prompt_height) / 2,
        width: prompt_width,
        height: prompt_height,
    };

    let server = app.config.active_server.as_deref().unwrap_or("(no server)");
    let port = app
        .config
        .effective_active_server_config()
        .map(|s| s.port.to_string())
        .unwrap_or_else(|| "-".to_string());
    let database = app
        .config
        .active_database
        .as_deref()
        .unwrap_or("(no database)");

    let connection_height = 3;
    let server_line = format!("{:<10}{}", "Server:", server);
    let port_line = format!("{:<10}{}", "Port:", port);
    let database_line = format!("{:<10}{}", "Database:", database);
    let content_width = [server_line.len(), port_line.len(), database_line.len()]
        .into_iter()
        .max()
        .unwrap_or(1) as u16;
    let max_connection_width = area.width.saturating_sub(2).max(1);
    let connection_width = content_width.saturating_add(2).min(max_connection_width);
    let connection_area = Rect {
        x: area.x + area.width.saturating_sub(connection_width) / 2,
        y: prompt_area
            .y
            .saturating_sub(connection_height)
            .saturating_sub(1),
        width: connection_width,
        height: connection_height,
    };

    let bottom_y = area.y + area.height.saturating_sub(1);
    let title_y = connection_area.y.saturating_sub(2);
    let footer_y = prompt_area
        .y
        .saturating_add(prompt_area.height)
        .saturating_add(1)
        .min(bottom_y);

    let title = Paragraph::new(format!(
        "M A R K L O G I C   T U I   (v{})",
        env!("CARGO_PKG_VERSION")
    ))
    .alignment(Alignment::Center)
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(
        title,
        Rect {
            x: area.x,
            y: title_y,
            width: area.width,
            height: 1,
        },
    );

    let label_style = Style::default().fg(Color::Gray);
    let server_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let port_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let database_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let connection_lines = vec![
        Line::from(vec![
            Span::styled(format!("{:<10}", "Server:"), label_style),
            Span::styled(server.to_string(), server_style),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10}", "Port:"), label_style),
            Span::styled(port, port_style),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10}", "Database:"), label_style),
            Span::styled(database.to_string(), database_style),
        ]),
    ];
    f.render_widget(Paragraph::new(connection_lines), connection_area);

    let footer_text = if app.status_message.is_empty() {
        "Ctrl+1–6: screens · Ctrl+.: previous · ':' commands · '?' help (Alt also works)"
    } else {
        app.status_message.as_str()
    };
    let footer = Paragraph::new(footer_text)
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(
        footer,
        Rect {
            x: area.x,
            y: footer_y,
            width: area.width,
            height: 1,
        },
    );

    let panel_bg = Color::Rgb(84, 84, 84);
    let command_style = Style::default().fg(Color::White).bg(panel_bg);
    f.render_widget(Block::default().style(command_style), prompt_area);
    let bar_style = Style::default()
        .fg(app.edit_mode_accent_color())
        .bg(panel_bg);

    let command_input = app
        .command_input
        .strip_prefix(':')
        .unwrap_or(app.command_input.as_str());

    let bar_glyph = "▌";
    let top_blank = Line::from(vec![Span::styled(bar_glyph, bar_style)]);
    f.render_widget(
        Paragraph::new(top_blank).style(command_style),
        Rect {
            x: prompt_area.x,
            y: prompt_area.y,
            width: prompt_area.width,
            height: 1,
        },
    );

    let command_line = Line::from(vec![
        Span::styled(bar_glyph, bar_style),
        Span::raw(" "),
        Span::raw(command_input.to_string()),
    ]);
    f.render_widget(
        Paragraph::new(command_line).style(command_style),
        Rect {
            x: prompt_area.x,
            y: prompt_area.y.saturating_add(1),
            width: prompt_area.width,
            height: 1,
        },
    );

    let middle_blank = Line::from(vec![Span::styled(bar_glyph, bar_style)]);
    f.render_widget(
        Paragraph::new(middle_blank).style(command_style),
        Rect {
            x: prompt_area.x,
            y: prompt_area.y.saturating_add(2),
            width: prompt_area.width,
            height: 1,
        },
    );

    if app.focus == Focus::Command {
        let cursor_offset =
            (command_input.chars().count() as u16 + 2).min(prompt_area.width.saturating_sub(1));
        let cursor_x = prompt_area.x + cursor_offset;
        f.set_cursor_position((cursor_x, prompt_area.y.saturating_add(1)));
    }

    if !app.autocomplete_suggestions.is_empty() && app.focus == Focus::Command {
        let items: Vec<ListItem> = app
            .autocomplete_suggestions
            .iter()
            .enumerate()
            .map(|(i, &cmd)| {
                let desc = App::COMMANDS
                    .iter()
                    .find(|(c, _)| *c == cmd)
                    .map(|(_, d)| *d)
                    .unwrap_or("");
                let indent = command_indent_level(cmd);
                let indent_str = "  ".repeat(indent);
                let text = format!("{}{} - {}", indent_str, cmd, desc);
                let style = if i == app.autocomplete_selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(text).style(style)
            })
            .collect();

        let height = (items.len() as u16 + 2).min(12);
        let popup_y = if prompt_area
            .y
            .saturating_add(prompt_area.height)
            .saturating_add(height)
            <= bottom_y.saturating_add(1)
        {
            prompt_area.y.saturating_add(prompt_area.height)
        } else {
            prompt_area.y.saturating_sub(height)
        };
        let popup_area = Rect {
            x: prompt_area.x,
            y: popup_y,
            width: prompt_area.width,
            height,
        };
        let list =
            List::new(items).block(Block::default().borders(Borders::ALL).title("Suggestions"));
        f.render_widget(Clear, popup_area);
        f.render_widget(list, popup_area);
    }
}

fn ui_normal(f: &mut Frame, app: &mut App) {
    // Command line is hidden unless focused, filtering, or has content
    let cmd_visible = app.focus == Focus::Command
        || app.focus == Focus::Filter
        || !app.command_input.is_empty()
        || app.uri_filter.is_some()
        || app.query_results_filter.is_some()
        || !app.status_message.is_empty();
    let cmd_height = if cmd_visible { 1 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // merged status + keybindings bar
            Constraint::Min(0),             // main area
            Constraint::Length(cmd_height), // command (at bottom, collapsible)
        ])
        .split(f.area());

    // Merged top bar: status first, keybindings flowing after it across the
    // full width. (A fixed 50/50 split with right alignment clipped the
    // start of the hints while the left half sat empty.)
    let menu_bar_style = Style::default()
        .bg(menu_bar_background_color(app.terminal_focused))
        .fg(menu_bar_text_color());
    let status_line = app.status_line();
    let status_width = (status_line.width() as u16)
        .saturating_add(1)
        .min(chunks[0].width);
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(status_width), Constraint::Min(0)])
        .split(chunks[0]);
    let status = Paragraph::new(status_line).style(menu_bar_style);
    f.render_widget(status, top_chunks[0]);
    let kb_line = if app.focus == Focus::Results
        && app.query_results_format == QueryResultsFormat::Uris
        && !app.query_results.is_empty()
    {
        styled_menu_bar_items(&[
            "Enter: open URI".to_string(),
            "Ctrl+D: delete URI".to_string(),
            "F: format".to_string(),
            "j/k: navigate".to_string(),
            "?: help".to_string(),
        ])
    } else {
        keybindings_line(
            &app.focus,
            &app.edit_mode,
            app.file_list_visible,
            app.active_sync_info().is_some(),
            app.modules_view,
        )
    };
    let kb_bar = Paragraph::new(kb_line).style(menu_bar_style);
    f.render_widget(kb_bar, top_chunks[1]);

    // Content area: query on top (if visible), results on bottom
    // The focused panel gets 80% of the vertical space.
    let main_chunks = if app.query_visible {
        let (query_pct, results_pct) = match app.focus {
            Focus::Query => (80, 20),
            Focus::Results => (20, 80),
            _ => (30, 70),
        };
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(query_pct),
                Constraint::Percentage(results_pct),
            ])
            .split(chunks[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0)])
            .split(chunks[1])
    };

    // Command / Filter input (at bottom)
    if cmd_visible {
        if app.focus == Focus::Filter {
            let filter_style = Style::default().fg(Color::White).bg(Color::Rgb(0, 90, 0));
            f.render_widget(Block::default().style(filter_style), chunks[2]);
            let filter_display = format!("/{}", app.filter_input);
            let filter_widget = Paragraph::new(filter_display.as_str()).style(filter_style);
            f.render_widget(filter_widget, chunks[2]);
            let cursor_x = chunks[2].x + app.filter_input.chars().count() as u16 + 1;
            let cursor_y = chunks[2].y;
            f.set_cursor_position((cursor_x, cursor_y));
        } else if !app.command_input.is_empty() || app.focus == Focus::Command {
            let command_style = Style::default().fg(Color::White).bg(Color::DarkGray);
            f.render_widget(Block::default().style(command_style), chunks[2]);

            let command_input = app
                .command_input
                .strip_prefix(':')
                .unwrap_or(app.command_input.as_str());
            let mut command_line = format!(":{}", command_input);
            if let Some(ref filter) = app.uri_filter {
                if !filter.is_empty() {
                    command_line.push_str(&format!("  [filter: {}]", filter));
                }
            }
            if let Some(ref filter) = app.query_results_filter {
                command_line.push_str(&format!("  [result filter: {}]", filter));
            }
            let command = Paragraph::new(command_line).style(command_style);
            f.render_widget(command, chunks[2]);

            // Show cursor in command input when focused
            if app.focus == Focus::Command {
                let cursor_x = chunks[2].x + command_input.chars().count() as u16 + 1;
                let cursor_y = chunks[2].y;
                f.set_cursor_position((cursor_x, cursor_y));
            }
        } else if !app.status_message.is_empty() {
            let status_style = Style::default().fg(Color::White).bg(Color::DarkGray);
            f.render_widget(Block::default().style(status_style), chunks[2]);
            let status_widget = Paragraph::new(app.status_message.as_str()).style(status_style);
            f.render_widget(status_widget, chunks[2]);
        }
    }

    let results_area = if app.query_visible {
        main_chunks[1]
    } else {
        main_chunks[0]
    };
    app.mouse_regions.push((results_area, MouseTarget::Results));
    if cmd_visible {
        app.mouse_regions.push((chunks[2], MouseTarget::Command));
    }

    if app.query_visible {
        app.mouse_regions.push((main_chunks[0], MouseTarget::Query));
        // Query panel: optionally split horizontally for file list
        // edtui manages its own modal state so we don't gate on EditMode::Navigate
        let should_show_file_list =
            app.file_list_visible && (app.use_edtui || app.edit_mode == EditMode::Navigate);
        let file_list_width = sidebar_width(app);
        let (editor_area, file_list_area) = if should_show_file_list {
            let hchunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(file_list_width)])
                .split(main_chunks[0]);
            (hchunks[0], Some(hchunks[1]))
        } else {
            (main_chunks[0], None)
        };

        // Query editor (left side, or full width if no file list)
        let border_style = panel_border_style(app.focus == Focus::Query, app.terminal_focused);
        let editor_bg = if app.edit_mode == EditMode::Insert {
            Color::Rgb(0, 0, 0)
        } else {
            app_background_color()
        };
        let query_block = if should_show_file_list {
            let title_line = Line::from(vec![Span::raw("[1]─ "), Span::raw(app.query_title())]);
            Block::default()
                .borders(Borders::LEFT | Borders::TOP | Borders::BOTTOM)
                .title(title_line)
                .border_style(border_style)
                .style(Style::default().bg(editor_bg))
        } else {
            panel_block(app.query_title(), "[1]")
                .border_style(border_style)
                .style(Style::default().bg(editor_bg))
        };
        if app.use_edtui {
            EditorView::new(&mut app.edtui_state)
                .theme(EditorTheme::default().block(query_block))
                .syntax_highlighter(crate::query_highlight::highlighter_for(
                    app.active_query_file.as_deref(),
                ))
                .line_numbers(LineNumbers::Absolute)
                .render(editor_area, f.buffer_mut());
        } else {
            app.query_editor.set_block(query_block);
            app.query_editor.set_cursor_line_style(Style::default());
            if app.focus == Focus::Query && app.edit_mode == EditMode::Insert {
                app.query_editor
                    .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
            } else {
                app.query_editor.set_cursor_style(Style::default());
            }
            f.render_widget(&app.query_editor, editor_area);
        }

        // File list (right side)
        if let Some(area) = file_list_area {
            let file_list_style = panel_border_style(
                app.focus == Focus::Query && (app.use_edtui || app.edit_mode == EditMode::Navigate),
                app.terminal_focused,
            );

            if app.modules_view {
                let rows = app.module_tree_rows();
                let items: Vec<ListItem> = if rows.is_empty() {
                    vec![ListItem::new("  (no cached modules — m to search)")]
                } else {
                    rows.iter()
                        .map(|row| {
                            let text = module_tree_item_text(app, row, area.width);
                            let style = if row.is_dir {
                                Style::default().fg(Color::DarkGray)
                            } else if app.running_queries.contains_key(&row.path) {
                                Style::default().fg(Color::Red)
                            } else if app.active_query_file.as_ref() == Some(&row.path) {
                                Style::default().fg(Color::Cyan)
                            } else if app
                                .module_status_for_working(&row.path)
                                .is_some_and(|status| status.unsynced)
                            {
                                Style::default().fg(Color::Yellow)
                            } else {
                                Style::default()
                            };
                            ListItem::new(text).style(style)
                        })
                        .collect()
                };

                let unsynced = app.cached_modules.iter().filter(|e| e.unsynced).count();
                let title = format!("Modules [*dirty ↑unsynced · s=sync({})]", unsynced);
                let list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::TOP | Borders::RIGHT | Borders::BOTTOM)
                            .title(title)
                            .border_style(file_list_style)
                            .style(Style::default().bg(Color::Rgb(26, 20, 44))),
                    )
                    .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
                    .highlight_symbol("> ");
                f.render_stateful_widget(list, area, &mut app.cached_modules_state);
            } else {
                let rows = app.query_file_rows();
                let items: Vec<ListItem> = rows
                    .iter()
                    .map(|row| {
                        let text = query_tree_item_text(app, row, area.width);
                        let style = if row.is_dir {
                            Style::default().fg(Color::DarkGray)
                        } else if app.running_queries.contains_key(&row.path) {
                            Style::default().fg(Color::Red)
                        } else if app.active_query_file.as_ref() == Some(&row.path) {
                            Style::default().fg(Color::Cyan)
                        } else {
                            Style::default()
                        };
                        ListItem::new(text).style(style)
                    })
                    .collect();

                let list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::TOP | Borders::RIGHT | Borders::BOTTOM)
                            .title("Files [f=folders, m=modules, M=rename]")
                            .border_style(file_list_style)
                            .style(Style::default().bg(Color::Rgb(10, 18, 42))),
                    )
                    .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
                    .highlight_symbol("> ");
                f.render_stateful_widget(list, area, &mut app.query_file_list_state);
            }
        }
    } // end if query_visible

    // Results area
    let results_style = panel_border_style(app.focus == Focus::Results, app.terminal_focused);

    // Dynamic page size based on available height (subtract borders + header row)
    let available_height = results_area.height.saturating_sub(4) as usize; // 2 borders + 1 header + 1 buffer
    if available_height > 0 {
        app.page_size = available_height;
    }

    if !app.records.is_empty() {
        // Show as table with aligned columns
        let rows: Vec<Row> = app
            .records
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let selected = app.selected_indices.contains(&i);
                let check = if selected { "[x]" } else { "[ ]" };
                let cols = if r.collections.is_empty() {
                    String::new()
                } else {
                    r.collections.join(", ")
                };
                let style = if selected {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else if i % 2 == 0 {
                    Style::default().bg(Color::Rgb(30, 30, 30))
                } else {
                    Style::default()
                };
                Row::new(vec![check.to_string(), r.uri.clone(), cols]).style(style)
            })
            .collect();

        let title = "Results";
        let header = Row::new(vec!["", "URI", "Collections"]).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        );
        let widths = [
            Constraint::Length(3),
            Constraint::Percentage(45),
            Constraint::Percentage(50),
        ];
        let total_str = app
            .total_results
            .map(|t| t.to_string())
            .unwrap_or("?".to_string());
        let page_title = format!("Page {} | Total: {}", app.current_page + 1, total_str);
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                panel_block(title, "[2]")
                    .border_style(results_style)
                    .title(Line::from(page_title).alignment(Alignment::Right)),
            )
            .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, results_area, &mut app.list_state);
    } else if app.query_visible && !app.query_results.is_empty() {
        let visible_count = app.visible_query_result_count();
        let count = if app.query_results_filter.is_some() {
            format!("{visible_count}/{} visible", app.query_results.len())
        } else {
            app.total_results
                .unwrap_or(app.query_results.len())
                .to_string()
        };
        let actions = if app.query_results_format == QueryResultsFormat::Uris {
            "Enter=open · Ctrl+D=delete · /=filter · F=format"
        } else {
            "/=filter · F=format"
        };
        let title = format!(
            "Query Results ({}) [{} · {}]",
            count,
            app.query_results_format.label(),
            actions
        );
        let mut block = panel_block(title, "[2]").border_style(results_style);
        if let Some(path) = app.active_query_file.as_ref()
            && let Some(elapsed) = app.running_query_elapsed(path)
        {
            block = block.title(running_title(elapsed));
        } else if let Some(ts) = app.query_results_timestamp {
            block = block.title(Line::from(format_relative_time(ts)).alignment(Alignment::Right));
        }
        match app.query_results_format {
            QueryResultsFormat::Json => {
                let visible = app
                    .visible_query_results()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let results = Paragraph::new(pretty_json_results(&visible))
                    .block(block)
                    .wrap(Wrap { trim: false });
                f.render_widget(results, results_area);
            }
            QueryResultsFormat::Uris => {
                let rows: Vec<Row> = app
                    .uri_query_results
                    .iter()
                    .enumerate()
                    .map(|(index, result)| {
                        let metadata_error = result.error.as_deref().unwrap_or_default();
                        let collections = if result.collections.is_empty() {
                            metadata_error.to_string()
                        } else {
                            result.collections.join(", ")
                        };
                        let permissions = result.permissions.join(", ");
                        let property_count = result
                            .property_count
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        let style = if index % 2 == 0 {
                            Style::default().bg(Color::Rgb(30, 30, 30))
                        } else {
                            Style::default()
                        };
                        Row::new(vec![
                            (index + 1).to_string(),
                            result.uri.clone(),
                            collections,
                            permissions,
                            property_count,
                        ])
                        .style(style)
                    })
                    .collect();
                let header = Row::new(vec![
                    "#",
                    "URI",
                    "Collections",
                    "Permissions",
                    "Property files",
                ])
                .style(
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Cyan),
                );
                let widths = [
                    Constraint::Length(4),
                    Constraint::Percentage(27),
                    Constraint::Percentage(25),
                    Constraint::Percentage(35),
                    Constraint::Length(12),
                ];
                let table = Table::new(rows, widths)
                    .header(header)
                    .block(block)
                    .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
                f.render_stateful_widget(table, results_area, &mut app.query_results_state);
            }
            QueryResultsFormat::Csv | QueryResultsFormat::Psv | QueryResultsFormat::Tsv => {
                let delimiter = match app.query_results_format {
                    QueryResultsFormat::Csv => ',',
                    QueryResultsFormat::Psv => '|',
                    QueryResultsFormat::Tsv => '\t',
                    _ => unreachable!(),
                };
                let visible = app
                    .visible_query_results()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let values = parse_delimited_results(&visible, delimiter);
                let column_count = values.iter().map(Vec::len).max().unwrap_or(1).max(1);
                let rows: Vec<Row> = values
                    .into_iter()
                    .enumerate()
                    .map(|(index, mut values)| {
                        values.resize(column_count, String::new());
                        let mut cells = vec![(index + 1).to_string()];
                        cells.extend(values);
                        let style = if index % 2 == 0 {
                            Style::default().bg(Color::Rgb(30, 30, 30))
                        } else {
                            Style::default()
                        };
                        Row::new(cells).style(style)
                    })
                    .collect();
                let mut headings = vec!["#".to_string()];
                headings.extend((1..=column_count).map(|number| format!("Column {number}")));
                let mut widths = vec![Constraint::Length(4)];
                widths.extend(std::iter::repeat_n(Constraint::Min(10), column_count));
                let table = Table::new(rows, widths)
                    .header(
                        Row::new(headings).style(
                            Style::default()
                                .add_modifier(Modifier::BOLD)
                                .fg(Color::Cyan),
                        ),
                    )
                    .block(block)
                    .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
                f.render_stateful_widget(table, results_area, &mut app.query_results_state);
            }
            QueryResultsFormat::Plain => {
                // Show query results as a navigable list with snippets.
                // width minus borders(2), index col(4), padding(3)
                let max_width = results_area.width.saturating_sub(9) as usize;
                let rows: Vec<Row> = app
                    .visible_query_results()
                    .into_iter()
                    .enumerate()
                    .map(|(index, result)| {
                        let snippet = make_snippet(result, max_width);
                        let style = if index % 2 == 0 {
                            Style::default().bg(Color::Rgb(30, 30, 30))
                        } else {
                            Style::default()
                        };
                        Row::new(vec![(index + 1).to_string(), snippet]).style(style)
                    })
                    .collect();
                let header = Row::new(vec!["#", "Result"]).style(
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Cyan),
                );
                let table = Table::new(rows, [Constraint::Length(4), Constraint::Min(0)])
                    .header(header)
                    .block(block)
                    .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
                f.render_stateful_widget(table, results_area, &mut app.query_results_state);
            }
        }
    } else {
        let mut block = panel_block("Results", "[2]").border_style(results_style);
        if let Some(path) = app.active_query_file.as_ref()
            && let Some(elapsed) = app.running_query_elapsed(path)
        {
            block = block.title(running_title(elapsed));
        }
        let text_style = if app.results_text == NO_OUTPUT_MESSAGE {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let formatted = if !app.query_visible && app.query_results_timestamp.is_some() {
            "Use :list to load documents.".to_string()
        } else {
            crate::query_error::format_message(&app.results_text)
        };
        let results = Paragraph::new(formatted)
            .style(text_style)
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(results, results_area);
    }

    // Autocomplete popup (rendered last so it draws on top of query area)
    if !app.autocomplete_suggestions.is_empty() && app.focus == Focus::Command {
        let items: Vec<ListItem> = app
            .autocomplete_suggestions
            .iter()
            .enumerate()
            .map(|(i, &cmd)| {
                let desc = App::COMMANDS
                    .iter()
                    .find(|(c, _)| *c == cmd)
                    .map(|(_, d)| *d)
                    .unwrap_or("");
                let indent = command_indent_level(cmd);
                let indent_str = "  ".repeat(indent);
                let text = format!("{}{} - {}", indent_str, cmd, desc);
                let style = if i == app.autocomplete_selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(text).style(style)
            })
            .collect();
        let height = (items.len() as u16 + 2).min(12); // +2 for borders
        let popup_area = Rect {
            x: chunks[2].x,
            y: chunks[2].y.saturating_sub(height),
            width: chunks[2].width.min(70),
            height,
        };
        let list =
            List::new(items).block(Block::default().borders(Borders::ALL).title("Suggestions"));
        f.render_widget(Clear, popup_area);
        f.render_widget(list, popup_area);
    }
}

fn ui_fullscreen(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(f.area());

    // Top row: status left, keybindings right
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    let total_lines = app.full_view_content.lines().count() as u16;
    let content_height = chunks[1].height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(content_height);
    let pct = if max_scroll == 0 {
        100
    } else {
        ((app.full_view_scroll as f32 / max_scroll as f32) * 100.0) as u16
    };
    app.fullscreen_area_height = chunks[1].height;

    let menu_bar_style = Style::default()
        .bg(menu_bar_background_color(app.terminal_focused))
        .fg(menu_bar_text_color());
    f.render_widget(
        Paragraph::new(app.status_line()).style(menu_bar_style),
        top_chunks[0],
    );

    let mut kb_items = vec![
        "q: close".to_string(),
        "j/k: scroll".to_string(),
        "d/u: page".to_string(),
        "gg/G: top/bottom".to_string(),
        "e: edit ext".to_string(),
        "m: edit doc".to_string(),
        "?: help".to_string(),
    ];
    kb_items.push(format!("{}%", pct));
    let kb_bar = Paragraph::new(styled_menu_bar_items(&kb_items))
        .style(menu_bar_style)
        .alignment(Alignment::Right);
    f.render_widget(kb_bar, top_chunks[1]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Document View");
    let para = Paragraph::new(app.full_view_content.as_str())
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.full_view_scroll, 0));
    f.render_widget(para, chunks[1]);
}

fn ui_collection_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(50, 60, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .collection_list
        .iter()
        .map(|col| {
            let marker = if app.current_collection.as_deref() == Some(col.as_str()) {
                " (active)"
            } else {
                ""
            };
            ListItem::new(format!("  {}{}", col, marker))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Collection"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.collection_list_state);
}

fn ui_query_result_format_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(48, 46, f.area());
    f.render_widget(Clear, area);

    let items = QueryResultsFormat::ALL
        .iter()
        .map(|format| {
            let active = if *format == app.query_results_format {
                " (current)"
            } else {
                ""
            };
            ListItem::new(format!("  {}{}", format.label(), active))
        })
        .chain(std::iter::once(ListItem::new("  edit (external editor)")))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Query Result Format  [Enter=apply, Esc=cancel]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.query_results_format_picker_state);
}

fn ui_active_variables(f: &mut Frame, app: &mut App) {
    let area = centered_rect(78, 60, f.area());
    f.render_widget(Clear, area);
    let rows = app
        .active_variable_fields
        .iter()
        .enumerate()
        .map(|(row, (name, value))| {
            let name_style = if app.active_variable_step == row * 2 {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };
            let value_style = if app.active_variable_step == row * 2 + 1 {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(name.clone()).style(name_style),
                Cell::from(value.clone()).style(value_style),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [Constraint::Percentage(35), Constraint::Percentage(65)],
    )
    .header(
        Row::new(vec!["Name", "Value"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(Block::default().borders(Borders::ALL).title(
        "Active Variables  [Tab=next field, Ctrl+N=add, Delete=remove, Enter=save, Esc=cancel]",
    ));
    f.render_widget(table, area);
}

fn ui_declared_variables(f: &mut Frame, app: &mut App) {
    let area = centered_rect(78, 60, f.area());
    f.render_widget(Clear, area);
    let rows = app
        .declared_variable_fields
        .iter()
        .enumerate()
        .map(|(row, (name, value))| {
            let value_style = if app.declared_variable_step == row {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(format!("${name}")),
                Cell::from(value.clone()).style(value_style),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [Constraint::Percentage(35), Constraint::Percentage(65)],
    )
    .header(
        Row::new(vec!["Variable", "Value (XQuery expression)"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Declared Variables  [Tab=next value, Enter=run with overrides, Esc=cancel]"),
    );
    f.render_widget(table, area);
}

fn ui_query_file_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);

    let rows = app.query_file_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let text = query_tree_item_text(app, row, area.width);
            let style = if row.is_dir {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Query File  [Enter=open/fold]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.query_file_list_state);
}

/// Untruncated label for one row of the Files tree: indent, fold icon, name,
/// and the active/running markers.
fn query_row_label(app: &App, row: &QueryTreeRow) -> String {
    let name = row
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let indent = "  ".repeat(row.depth);
    if row.is_dir {
        let icon = if app.collapsed_query_folders.contains(&row.path) {
            "▸ "
        } else {
            "▾ "
        };
        format!("{indent}{icon}{name}")
    } else {
        let is_active = app.active_query_file.as_ref() == Some(&row.path);
        let is_running = app.running_queries.contains_key(&row.path);
        let marker = if is_active { " *" } else { "" };
        let running_marker = if is_running { " (running)" } else { "" };
        format!("{indent}  {name}{marker}{running_marker}")
    }
}

/// Untruncated label for one row of the Modules tree: indent, fold icon, name,
/// and the dirty (`*`) / unsynced (`↑`) flags. Mirrors [`query_row_label`].
fn module_row_label(app: &App, row: &QueryTreeRow) -> String {
    let name = row
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let indent = "  ".repeat(row.depth);
    if row.is_dir {
        let icon = if app.collapsed_module_folders.contains(&row.path) {
            "▸ "
        } else {
            "▾ "
        };
        format!("{indent}{icon}{name}")
    } else {
        let flags = app
            .module_status_for_working(&row.path)
            .map(|status| {
                format!(
                    "{}{}",
                    if status.dirty { "*" } else { "" },
                    if status.unsynced { "↑" } else { "" }
                )
            })
            .unwrap_or_default();
        let is_running = app.running_queries.contains_key(&row.path);
        let running_marker = if is_running { " (running)" } else { "" };
        format!("{indent}  {name}{flags}{running_marker}")
    }
}

/// Files tree label, truncated to fit inside `outer_width` (borders and the
/// highlight symbol excluded).
fn query_tree_item_text(app: &App, row: &QueryTreeRow, outer_width: u16) -> String {
    let available = outer_width.saturating_sub(4) as usize;
    truncate_with_ellipsis(&query_row_label(app, row), available)
}

/// Modules tree label, truncated like [`query_tree_item_text`].
fn module_tree_item_text(app: &App, row: &QueryTreeRow, outer_width: u16) -> String {
    let available = outer_width.saturating_sub(4) as usize;
    truncate_with_ellipsis(&module_row_label(app, row), available)
}

/// Width of the Files and Modules sidebar. Both views use this one width, so
/// it does not change when switching between them. It is the widest label in
/// either tree (folding ignored, so the width stays put when folders toggle),
/// plus the selection marker and borders.
pub(crate) fn sidebar_width(app: &App) -> u16 {
    let expanded = HashSet::new();
    let file_rows = build_query_tree_rows(
        &app.query_root_dir,
        &app.query_files,
        &app.query_folders,
        &expanded,
    );
    let module_rows = app.module_tree_rows_with(&expanded);
    let widest = file_rows
        .iter()
        .map(|row| query_row_label(app, row).chars().count())
        .chain(
            module_rows
                .iter()
                .map(|row| module_row_label(app, row).chars().count()),
        )
        .max()
        .unwrap_or(0);
    file_list_width(widest)
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let kept: String = text.chars().take(max_chars - 1).collect();
    format!("{kept}…")
}

fn ui_query_file_create(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);

    let input = Paragraph::new(app.new_query_file_input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title("New Query File"),
    );
    f.render_widget(input, area);
    f.set_cursor_position((
        area.x + app.new_query_file_input.len() as u16 + 1,
        area.y + 1,
    ));
}

fn ui_query_file_rename(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);

    let input = Paragraph::new(app.rename_file_input.as_str())
        .block(Block::default().borders(Borders::ALL).title("Rename File"));
    f.render_widget(input, area);
    f.set_cursor_position((area.x + app.rename_file_input.len() as u16 + 1, area.y + 1));
}

fn ui_query_file_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);

    let file_name = app
        .file_delete_target
        .as_ref()
        .map(|p| display_query_path(p, &app.query_root_dir))
        .unwrap_or_else(|| "(unknown)".to_string());

    let text = format!(
        "Delete query file '{}' ?\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        file_name
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm File Delete")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_multi_run_form(f: &mut Frame, app: &App) {
    let labels = ["Runs (1-1000)", "Wait seconds between runs"];
    let area = centered_rect(60, 26, f.area());
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            labels
                .iter()
                .map(|_| Constraint::Length(3))
                .chain(std::iter::once(Constraint::Min(0)))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (i, label) in labels.iter().enumerate() {
        let style = panel_border_style(i == app.multi_run_form_step, app.terminal_focused);
        let display = app
            .multi_run_form_fields
            .get(i)
            .cloned()
            .unwrap_or_default();
        let p = Paragraph::new(display).block(
            Block::default()
                .borders(Borders::ALL)
                .title(*label)
                .border_style(style),
        );
        f.render_widget(p, chunks[i]);
    }

    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Multi-Run Query [Tab fields, Enter start, Esc cancel]")
            .border_style(active_border_style(true, app.terminal_focused, Color::Cyan)),
        area,
    );

    let step = app.multi_run_form_step.min(1);
    let field_area = chunks[step];
    let field_len = app
        .multi_run_form_fields
        .get(step)
        .map(|s| s.chars().count())
        .unwrap_or(0) as u16;
    f.set_cursor_position((
        (field_area.x + field_len + 1).min(field_area.x + field_area.width.saturating_sub(1)),
        field_area.y + 1,
    ));
}

fn ui_module_clone_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(90, 90, f.area());
    f.render_widget(Clear, area);

    // Staged pane grows with the selection, capped so results keep room.
    let staged_height = if app.module_search.marked.is_empty() {
        4u16
    } else {
        (app.module_search.marked.len() as u16 + 2).clamp(4, 10)
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(staged_height),
        ])
        .split(area);

    let modules_db = app
        .config
        .active_modules_database
        .as_deref()
        .unwrap_or("(no modules db)");
    let input_title = format!("Module Pattern (db: {})", modules_db);
    let input_block = Block::default()
        .borders(Borders::ALL)
        .title(input_title)
        .border_style(panel_border_style(
            app.module_search.focus_input,
            app.terminal_focused,
        ));
    let filter = Paragraph::new(app.module_search.pattern.as_str()).block(input_block);
    f.render_widget(filter, rows[0]);

    let items: Vec<ListItem> = app
        .module_search
        .results
        .iter()
        .map(|uri| {
            let mark = if app.module_search.marked.contains(uri) {
                "[x]"
            } else {
                "[ ]"
            };
            ListItem::new(format!("  {} {}", mark, uri))
        })
        .collect();

    let total_str = app
        .module_search
        .total
        .map(|t| t.to_string())
        .unwrap_or_else(|| "?".to_string());
    let page_str = if app.module_search.results.is_empty() {
        "0".to_string()
    } else {
        (app.module_search.page + 1).to_string()
    };
    let list_title = format!(
        "Modules [Space=mark, S/Enter=download marked ({}), a=all, u=clear, Tab=focus, n/p=page] | Page {} / Total: {}",
        app.module_search.marked.len(),
        page_str,
        total_str
    );
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(list_title)
        .border_style(panel_border_style(
            !app.module_search.focus_input,
            app.terminal_focused,
        ));
    let list = List::new(items)
        .block(list_block)
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, rows[1], &mut app.module_search.selected);

    let staged_items: Vec<ListItem> = if app.module_search.marked.is_empty() {
        vec![
            ListItem::new("  (nothing marked yet — Space to mark, marks survive new searches)")
                .style(Style::default().fg(Color::DarkGray)),
        ]
    } else {
        app.module_search
            .marked
            .iter()
            .enumerate()
            .map(|(i, uri)| ListItem::new(format!("  {}. {}", i + 1, uri)))
            .collect()
    };
    let staged = List::new(staged_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "Selected ({}) [S/Enter=download, u=clear]",
                app.module_search.marked.len()
            ))
            .border_style(active_border_style(true, app.terminal_focused, Color::Cyan)),
    );
    f.render_widget(staged, rows[2]);

    if app.module_search.focus_input {
        let cursor_x = rows[0].x + app.module_search.pattern.chars().count() as u16 + 1;
        f.set_cursor_position((cursor_x, rows[0].y + 1));
    }
}

fn ui_module_reset_confirm(f: &mut Frame, _app: &App) {
    let area = centered_rect(70, 24, f.area());
    f.render_widget(Clear, area);

    let text = "Reset remote module to the original checkout.\n\nPress 'y' to reset and keep the local file, 'd' to reset and drop the local checkout, 'n' or Esc to cancel.";
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Module Reset")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_module_drop_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 24, f.area());
    f.render_widget(Clear, area);

    let text = if app.module_drop_needs_reset {
        "Local module differs from the original checkout.\n\nPress 'y' to reset the remote module first, then drop the local file. Press 'n' to drop the local file only. Press Esc to cancel."
    } else {
        "Drop the local module checkout?\n\nPress 'y' to confirm, 'n' or Esc to cancel."
    };
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Module Drop")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_module_diff_view(f: &mut Frame, app: &mut App) {
    let area = centered_rect(90, 90, f.area());
    f.render_widget(Clear, area);

    let title = format!(
        "Diff: {} [j/k scroll, g/G top/bottom, q/Esc close]",
        app.module_diff_uri.as_deref().unwrap_or("(unknown)")
    );
    let lines: Vec<Line> = app
        .module_diff_lines
        .iter()
        .map(|dl| {
            let (prefix, style) = match dl.kind {
                crate::modules_cache::DiffKind::Same => {
                    ("  ", Style::default().fg(Color::DarkGray))
                }
                crate::modules_cache::DiffKind::Del => ("- ", Style::default().fg(Color::Red)),
                crate::modules_cache::DiffKind::Add => ("+ ", Style::default().fg(Color::Green)),
            };
            Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(dl.text.clone(), style),
            ])
        })
        .collect();
    let total = lines.len();
    let visible = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(visible);
    app.module_diff_scroll = app.module_diff_scroll.min(max_scroll as u16);
    let start = app.module_diff_scroll as usize;
    let end = (start + visible).min(total);
    let para = Paragraph::new(lines[start..end].to_vec())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(panel_border_style(true, app.terminal_focused)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_module_drop_cache_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 30, f.area());
    f.render_widget(Clear, area);

    let (uri, dirty, unsynced) = app
        .module_drop_target
        .as_ref()
        .map(|(u, d, s)| (u.as_str(), *d, *s))
        .unwrap_or(("(unknown)", false, false));
    let mut warnings = Vec::new();
    if dirty {
        warnings.push("its working copy differs from the original checkout");
    }
    if unsynced {
        warnings.push("local content was never synced to the server");
    }
    let warning = if warnings.is_empty() {
        "The server copy is unaffected.".to_string()
    } else {
        format!(
            "Warning: {} — that content exists only here.\nThe server copy is unaffected.",
            warnings.join(" and ")
        )
    };
    let text = format!(
        "Drop the local cached copy of '{}' ?\n\n{}\n\nPress 'y' to confirm, 'n' or Esc to cancel.",
        uri, warning
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Drop Cached Copy")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_module_create(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 24, f.area());
    f.render_widget(Clear, area);

    let modules_db = app
        .config
        .active_modules_database
        .as_deref()
        .unwrap_or("(no modules db)");
    let input = Paragraph::new(app.new_module_uri_input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("New Module URI (db: {})", modules_db)),
    );
    f.render_widget(input, area);
    f.set_cursor_position((
        area.x + app.new_module_uri_input.len() as u16 + 1,
        area.y + 1,
    ));
}

fn ui_tracked_folder_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .tracked_folder_items
        .iter()
        .map(|item| match item {
            TrackedFolderMenuItem::Header(title) => ListItem::new(format!("  {}", title)).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TrackedFolderMenuItem::Folder(entry) => {
                let path = Path::new(&entry.path);
                let active_marker = if path == app.query_root_dir {
                    " (active)"
                } else {
                    ""
                };
                let favorite_marker = if entry.favorite { " [fav]" } else { "" };
                let last_accessed = entry
                    .last_accessed
                    .map(|secs| format_relative_time(UNIX_EPOCH + Duration::from_secs(secs)))
                    .unwrap_or_else(|| "never".to_string());
                ListItem::new(format!(
                    "  {}{}{} · {}",
                    display_folder_path(path),
                    favorite_marker,
                    active_marker,
                    last_accessed
                ))
            }
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Folders [Enter=switch, a=add, f=favorite, d=remove, c=clear cache]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.tracked_folder_list_state);
}

fn ui_tracked_folder_add(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 20, f.area());
    f.render_widget(Clear, area);

    let input = Paragraph::new(app.tracked_folder_input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Add Folder (~, absolute, or relative path)"),
    );
    f.render_widget(input, area);
    f.set_cursor_position((
        area.x + app.tracked_folder_input.len() as u16 + 1,
        area.y + 1,
    ));
}

fn ui_tracked_folder_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 20, f.area());
    f.render_widget(Clear, area);

    let folder = app
        .tracked_folder_delete_target
        .as_deref()
        .map(display_folder_path)
        .unwrap_or_else(|| "(unknown)".to_string());
    let text = format!(
        "Remove tracked folder '{}' ?\n\nThis does not delete files on disk.\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        folder
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Folder Remove")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_tracked_folder_cache_clear_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 22, f.area());
    f.render_widget(Clear, area);

    let folder = app
        .tracked_folder_cache_clear_target
        .as_deref()
        .map(display_folder_path)
        .unwrap_or_else(|| "(unknown)".to_string());
    let text = format!(
        "Clear cached query results for '{}' ?\n\nThis deletes that folder's .marklogic-tui cache directory.\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        folder
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Cache Clear")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_delete_confirm(f: &mut Frame, app: &App) {
    let uris = app.delete_uris();
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);

    let mut lines = vec![format!("Delete {} document(s)?", uris.len()), String::new()];
    for uri in &uris {
        lines.push(format!("  {}", uri));
    }
    lines.push(String::new());
    lines.push("Press 'y' to confirm, 'n' or Esc to cancel".to_string());

    let text = lines.join("\n");
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Delete")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_servers_interface(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    let menu_bar_style = Style::default()
        .bg(menu_bar_background_color(app.terminal_focused))
        .fg(menu_bar_text_color());
    f.render_widget(
        Paragraph::new(app.status_line()).style(menu_bar_style),
        top_chunks[0],
    );
    let servers_kb_items = match app.selected_servers_interface_row() {
        Some(ServersInterfaceRow::Server { .. }) => vec![
            "Tab pane".to_string(),
            "Enter fold/open".to_string(),
            "s activate".to_string(),
            "a add".to_string(),
            "e edit".to_string(),
            "d remove".to_string(),
            "r discover".to_string(),
            "q close".to_string(),
        ],
        Some(ServersInterfaceRow::Group { .. }) => vec![
            "Tab pane".to_string(),
            "s activate server".to_string(),
            "a add".to_string(),
            "e edit server".to_string(),
            "d remove server".to_string(),
            "r discover".to_string(),
            "q close".to_string(),
        ],
        Some(ServersInterfaceRow::AppServer { .. }) => vec![
            "Tab pane".to_string(),
            "Enter activate".to_string(),
            "Ctrl+F favorite".to_string(),
            "a add".to_string(),
            "e edit".to_string(),
            "d remove".to_string(),
            "r discover".to_string(),
            "q close".to_string(),
        ],
        None => vec![
            "Tab pane".to_string(),
            "a add".to_string(),
            "q close".to_string(),
        ],
    };
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&servers_kb_items))
            .style(menu_bar_style)
            .alignment(Alignment::Right),
        top_chunks[1],
    );

    let favorites_height = 12u16.min(chunks[1].height.saturating_sub(3));
    let content_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(favorites_height), Constraint::Min(3)])
        .split(chunks[1]);
    app.mouse_regions
        .push((content_chunks[0], MouseTarget::ServerFavorites));
    app.mouse_regions
        .push((content_chunks[1], MouseTarget::ServerTree));

    let favorite_items: Vec<ListItem> = if app.servers_interface_favorites.is_empty() {
        vec![ListItem::new("  Select an app server and press Ctrl+F")]
    } else {
        app.servers_interface_favorites
            .iter()
            .map(|row| {
                let ServersInterfaceRow::AppServer {
                    server_name,
                    group_name,
                    app_server_name,
                } = row
                else {
                    return ListItem::new("");
                };
                let active = app.config.active_server.as_deref() == Some(server_name.as_str())
                    && app.config.active_app_server.as_deref() == Some(app_server_name.as_str());
                ListItem::new(format!(
                    "  {} → {} / {}{}",
                    server_name,
                    group_name,
                    app_server_name,
                    if active { " (active)" } else { "" },
                ))
            })
            .collect()
    };
    let favorites = List::new(favorite_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    "Favorites ({})",
                    app.servers_interface_favorites.len()
                ))
                .border_style(panel_border_style(
                    app.servers_interface_focus == ServersInterfaceFocus::Favorites,
                    app.terminal_focused,
                )),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(
        favorites,
        content_chunks[0],
        &mut app.servers_interface_favorites_state,
    );

    let server_table_rows: Vec<Row> = if app.servers_interface_tree.is_empty() {
        vec![
            Row::new(vec![
                Cell::from("No servers configured — press a to add one"),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(Style::default().fg(Color::DarkGray)),
        ]
    } else {
        app.servers_interface_tree
            .iter()
            .map(|row| match row {
                ServersInterfaceRow::Server { server_name } => {
                    let server = app
                        .config
                        .servers
                        .iter()
                        .find(|server| &server.name == server_name);
                    let expanded = app.expanded_servers.contains(server_name);
                    let active = app.config.active_server.as_deref() == Some(server_name.as_str());
                    Row::new(vec![
                        Cell::from(format!(
                            "{} {}{}",
                            if expanded { "▾" } else { "▸" },
                            server_name,
                            if active { " (active)" } else { "" },
                        )),
                        Cell::from(
                            server
                                .map(|server| server.port.to_string())
                                .unwrap_or_default(),
                        ),
                        Cell::from(
                            server
                                .map(|server| if server.secure { "yes" } else { "no" })
                                .unwrap_or("-"),
                        ),
                        Cell::from(if active { "yes" } else { "no" }),
                        Cell::from(""),
                        Cell::from(""),
                    ])
                    .style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                }
                ServersInterfaceRow::Group { group_name, .. } => Row::new(vec![
                    Cell::from(format!("  + {}", group_name)),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ])
                .style(Style::default().fg(Color::Cyan)),
                ServersInterfaceRow::AppServer {
                    server_name,
                    group_name,
                    app_server_name,
                } => {
                    let endpoint = app
                        .config
                        .servers
                        .iter()
                        .find(|server| &server.name == server_name)
                        .and_then(|server| {
                            server.app_servers.iter().find(|app_server| {
                                &app_server.group == group_name
                                    && &app_server.name == app_server_name
                            })
                        });
                    let active = app.config.active_server.as_deref() == Some(server_name.as_str())
                        && app.config.active_app_server.as_deref()
                            == Some(app_server_name.as_str());
                    Row::new(vec![
                        Cell::from(format!(
                            "    - {}{}",
                            app_server_name,
                            if active { " (active)" } else { "" },
                        )),
                        Cell::from(
                            endpoint
                                .map(|endpoint| endpoint.port.to_string())
                                .unwrap_or_default(),
                        ),
                        Cell::from(
                            endpoint
                                .map(|endpoint| if endpoint.secure { "yes" } else { "no" })
                                .unwrap_or("-"),
                        ),
                        Cell::from(if active { "yes" } else { "no" }),
                        Cell::from(
                            endpoint
                                .and_then(|endpoint| endpoint.content_database.as_deref())
                                .unwrap_or("-"),
                        ),
                        Cell::from(
                            endpoint
                                .and_then(|endpoint| endpoint.modules_database.as_deref())
                                .unwrap_or("-"),
                        ),
                    ])
                }
            })
            .collect()
    };
    let server_table = Table::new(
        server_table_rows,
        [
            Constraint::Percentage(32),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Percentage(20),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("Name"),
            Cell::from("Port"),
            Cell::from("Secure"),
            Cell::from("Active"),
            Cell::from("Content DB"),
            Cell::from("Modules DB"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Servers")
            .border_style(panel_border_style(
                app.servers_interface_focus == ServersInterfaceFocus::Tree,
                app.terminal_focused,
            )),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(
        server_table,
        content_chunks[1],
        &mut app.servers_interface_tree_state,
    );

    if !app.status_message.is_empty() {
        f.render_widget(
            Paragraph::new(app.status_message.as_str())
                .style(Style::default().fg(Color::White).bg(Color::DarkGray)),
            chunks[2],
        );
    }
}

fn ui_server_form(f: &mut Frame, app: &App) {
    let labels = [
        "Name",
        "Host (e.g. localhost)",
        "Username",
        "Password",
        "Auth Type",
        "Insecure SSL (y/n)",
    ];
    let area = centered_rect(60, 50, f.area());
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            labels
                .iter()
                .map(|_| Constraint::Length(3))
                .chain(std::iter::once(Constraint::Min(0)))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (i, label) in labels.iter().enumerate() {
        let style = panel_border_style(i == app.server_form_step, app.terminal_focused);
        let display = if i == 3 {
            "*".repeat(app.server_form_fields[i].len())
        } else {
            app.server_form_fields[i].clone()
        };
        let p = Paragraph::new(display).block(
            Block::default()
                .borders(Borders::ALL)
                .title(*label)
                .border_style(style),
        );
        f.render_widget(p, chunks[i]);
    }

    let title = match app.server_form_mode {
        ServerFormMode::Add => "Add Server [Tab fields, Enter save, Esc cancel]",
        ServerFormMode::Edit => "Edit Server [Tab fields, Enter save, Esc cancel]",
    };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(active_border_style(true, app.terminal_focused, Color::Cyan)),
        area,
    );

    let field_area = chunks[app.server_form_step];
    let cursor_x =
        field_area.x + app.server_form_fields[app.server_form_step].chars().count() as u16 + 1;
    f.set_cursor_position((
        cursor_x.min(field_area.x + field_area.width.saturating_sub(1)),
        field_area.y + 1,
    ));
}

fn ui_app_server_form(f: &mut Frame, app: &App) {
    let labels = [
        "Name",
        "Port",
        "SSL (y/n)",
        "Content Database",
        "Modules Database",
    ];
    let area = centered_rect(60, 50, f.area());
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            labels
                .iter()
                .map(|_| Constraint::Length(3))
                .chain(std::iter::once(Constraint::Min(0)))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (i, label) in labels.iter().enumerate() {
        let style = panel_border_style(i == app.app_server_form_step, app.terminal_focused);
        let display = app.app_server_form_fields[i].clone();
        let p = Paragraph::new(display).block(
            Block::default()
                .borders(Borders::ALL)
                .title(*label)
                .border_style(style),
        );
        f.render_widget(p, chunks[i]);
    }

    let title = match app.app_server_form_mode {
        ServerFormMode::Add => "Add App Server [Tab fields, Enter save, Esc cancel]",
        ServerFormMode::Edit => "Edit App Server [Tab fields, Enter save, Esc cancel]",
    };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(active_border_style(true, app.terminal_focused, Color::Cyan)),
        area,
    );

    let field_area = chunks[app.app_server_form_step];
    let cursor_x = field_area.x
        + app.app_server_form_fields[app.app_server_form_step]
            .chars()
            .count() as u16
        + 1;
    f.set_cursor_position((
        cursor_x.min(field_area.x + field_area.width.saturating_sub(1)),
        field_area.y + 1,
    ));
}

fn ui_app_server_delete_confirm(f: &mut Frame, app: &App) {
    let target = app
        .app_server_delete_target
        .as_deref()
        .unwrap_or("(unknown app server)");
    let text = format!(
        "Remove app server '{}' ?\n\nThis only removes the saved configuration.\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        target
    );
    let area = centered_rect(50, 20, f.area());
    f.render_widget(Clear, area);
    let paragraph = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Confirm Delete")
            .border_style(Style::default().fg(Color::Red)),
    );
    f.render_widget(paragraph, area);
}

fn ui_app_server_discovery(f: &mut Frame, app: &mut App) {
    let area = centered_rect(92, 86, f.area());
    f.render_widget(Clear, area);
    let server_name = app
        .app_server_discovery_parent
        .as_deref()
        .unwrap_or("(unknown server)");
    let rows = app
        .app_server_discovery_results
        .iter()
        .map(|endpoint| {
            let marked = app
                .app_server_discovery_marked
                .contains(&App::app_server_key(&endpoint.group, &endpoint.name));
            Row::new(vec![
                Cell::from(if marked { "[x]" } else { "[ ]" }),
                Cell::from(endpoint.name.clone()),
                Cell::from(endpoint.group.clone()),
                Cell::from(endpoint.port.to_string()),
                Cell::from(if endpoint.secure { "yes" } else { "no" }),
                Cell::from(
                    endpoint
                        .content_database
                        .clone()
                        .unwrap_or_else(|| "-".to_string()),
                ),
                Cell::from(
                    endpoint
                        .modules_database
                        .clone()
                        .unwrap_or_else(|| "-".to_string()),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Percentage(22),
            Constraint::Percentage(16),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Percentage(20),
            Constraint::Min(12),
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("Mark"),
            Cell::from("Name"),
            Cell::from("Group"),
            Cell::from("Port"),
            Cell::from("Secure"),
            Cell::from("Content DB"),
            Cell::from("Modules DB"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "Discovered App Servers for '{}' [Space mark, a all, u clear, Enter save, Esc cancel]",
                server_name
            ))
            .border_style(active_border_style(true, app.terminal_focused, Color::Cyan)),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(table, area, &mut app.app_server_discovery_state);
}

fn ui_document_create(f: &mut Frame, app: &App) {
    let labels = ["URI", "Collections (comma separated)", "Quality", "Content"];
    let area = f.area();
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    for (i, label) in labels.iter().enumerate() {
        let style = panel_border_style(i == app.doc_form_step, app.terminal_focused);
        let display = match i {
            0 => app.doc_form_uri.clone(),
            1 => app.doc_form_collections.clone(),
            2 => app.doc_form_quality.clone(),
            3 => app.doc_form_content.clone(),
            _ => String::new(),
        };
        let p = Paragraph::new(display).block(
            Block::default()
                .borders(Borders::ALL)
                .title(*label)
                .border_style(style),
        );
        f.render_widget(p, chunks[i]);
    }

    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Create Document [Tab fields, Enter save, Esc cancel]")
            .border_style(active_border_style(true, app.terminal_focused, Color::Cyan)),
        area,
    );

    let field_area = chunks[app.doc_form_step];
    let cursor_x = match app.doc_form_step {
        0 => field_area.x + app.doc_form_uri.chars().count() as u16 + 1,
        1 => field_area.x + app.doc_form_collections.chars().count() as u16 + 1,
        2 => field_area.x + app.doc_form_quality.chars().count() as u16 + 1,
        3 => field_area.x + app.doc_form_content.chars().count() as u16 + 1,
        _ => field_area.x + 1,
    };
    f.set_cursor_position((
        cursor_x.min(field_area.x + field_area.width.saturating_sub(2)),
        field_area.y + 1,
    ));
}

fn ui_document_metadata_edit(f: &mut Frame, app: &App) {
    let labels = ["URI", "Collections (comma separated)", "Quality", "Content"];
    let area = f.area();
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    for (i, label) in labels.iter().enumerate() {
        let style = panel_border_style(i == app.doc_form_step, app.terminal_focused);
        let display = match i {
            0 => app.doc_form_uri.clone(),
            1 => app.doc_form_collections.clone(),
            2 => app.doc_form_quality.clone(),
            3 => app.doc_form_content.clone(),
            _ => String::new(),
        };
        let p = Paragraph::new(display).block(
            Block::default()
                .borders(Borders::ALL)
                .title(*label)
                .border_style(style),
        );
        f.render_widget(p, chunks[i]);
    }

    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Edit Document [Tab fields, Enter save, Esc cancel]")
            .border_style(active_border_style(true, app.terminal_focused, Color::Cyan)),
        area,
    );

    let field_area = chunks[app.doc_form_step];
    let cursor_x = match app.doc_form_step {
        0 => field_area.x + app.doc_form_uri.chars().count() as u16 + 1,
        1 => field_area.x + app.doc_form_collections.chars().count() as u16 + 1,
        2 => field_area.x + app.doc_form_quality.chars().count() as u16 + 1,
        3 => field_area.x + app.doc_form_content.chars().count() as u16 + 1,
        _ => field_area.x + 1,
    };
    f.set_cursor_position((
        cursor_x.min(field_area.x + field_area.width.saturating_sub(2)),
        field_area.y + 1,
    ));
}

fn ui_server_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);

    let name = app
        .server_delete_target
        .as_deref()
        .unwrap_or("(unknown server)");
    let text = format!(
        "Remove server '{}' ?\n\nThis only removes the saved configuration.\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        name
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Server Remove")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_help(f: &mut Frame, app: &mut App) {
    let area = centered_rect(60, 70, f.area());
    f.render_widget(Clear, area);

    // The overlay replaced app.mode, so use the mode the user came from.
    let ctx = app.previous_mode.as_ref().unwrap_or(&app.mode);
    let sections = crate::help::sections_for_context(
        ctx,
        &app.focus,
        &app.edit_mode,
        app.file_list_visible,
        app.active_sync_info().is_some(),
        app.modules_view,
    );
    let lines: Vec<_> = crate::help::render_help_lines(&sections)
        .into_iter()
        .flat_map(|line| {
            crate::logs::presentation::wrap_line(
                Line::from(line),
                usize::from(area.width.saturating_sub(2)).max(1),
            )
        })
        .collect();
    app.help_scroll = app.help_scroll.min(
        lines
            .len()
            .saturating_sub(usize::from(area.height.saturating_sub(2))) as u16,
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Keybindings · j/k scroll · d/u page")
        .border_style(active_border_style(true, app.terminal_focused, Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .scroll((app.help_scroll, 0))
        .style(Style::default().fg(Color::Gray));

    f.render_widget(para, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimited_rows_respect_quoted_delimiters_and_escaped_quotes() {
        assert_eq!(
            parse_delimited_row(r#"one,"two, three","said ""hello""""#, ','),
            vec!["one", "two, three", "said \"hello\""]
        );
        assert_eq!(
            parse_delimited_results(&["a|b\nc|d".to_string()], '|'),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d".to_string()],
            ]
        );
    }

    #[test]
    fn json_results_are_pretty_printed_without_losing_non_json_values() {
        assert_eq!(
            pretty_json_results(&[r#"{"a":1}"#.to_string()]),
            "{\n  \"a\": 1\n}"
        );
        assert_eq!(pretty_json_results(&["not json".to_string()]), "not json");
    }

    #[test]
    fn make_snippet_collapses_blank_lines_and_trims() {
        assert_eq!(
            make_snippet("  first line \n\n   second   line\nthird", 100),
            "first line second   line third"
        );
    }

    #[test]
    fn make_snippet_truncates_to_width_with_ellipsis() {
        assert_eq!(make_snippet("abcdefghij", 4), "abcd...");
    }

    #[test]
    fn make_snippet_keeps_content_that_fits_without_ellipsis() {
        assert_eq!(make_snippet("abcde", 5), "abcde");
        assert_eq!(make_snippet("abc", 10), "abc");
    }

    #[test]
    fn make_snippet_handles_empty_and_zero_width() {
        assert_eq!(make_snippet("", 10), "");
        assert_eq!(make_snippet("\n\n   \n", 10), "");
        assert_eq!(make_snippet("anything", 0), "...");
    }

    #[test]
    fn make_snippet_truncates_on_char_boundaries() {
        // Each character is multi-byte; slicing by bytes would panic.
        assert_eq!(make_snippet("λμνξο", 3), "λμν...");
    }
}
