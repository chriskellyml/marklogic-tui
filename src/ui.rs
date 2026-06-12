use super::*;
use std::time::UNIX_EPOCH;

/// Format a DocumentDetail for the full-screen view with metadata header + content
/// Create a single-line snippet from a result string, truncated to max_len
fn make_snippet(text: &str, max_len: usize) -> String {
    // Collapse to single line
    let oneline: String = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if oneline.len() > max_len {
        format!("{}...", &oneline[..max_len])
    } else {
        oneline
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
    f.render_widget(
        Block::default().style(Style::default().bg(app_background_color())),
        f.area(),
    );

    match app.mode {
        AppMode::StartPage => ui_start_page(f, app),
        AppMode::FullScreenView => ui_fullscreen(f, app),
        AppMode::LogViewer => ui_log_viewer(f, app),
        AppMode::Interface(AppInterface::Servers) => ui_servers_interface(f, app),
        AppMode::ServerForm => ui_server_form(f, app),
        AppMode::ServerDeleteConfirm => ui_server_delete_confirm(f, app),
        AppMode::AppServerList => ui_app_server_list(f, app),
        AppMode::AppServerForm => ui_app_server_form(f, app),
        AppMode::AppServerDeleteConfirm => ui_app_server_delete_confirm(f, app),
        AppMode::CollectionSelect => ui_collection_select(f, app),
        AppMode::DeleteConfirm => ui_delete_confirm(f, app),
        AppMode::QueryFileSelect => ui_query_file_select(f, app),
        AppMode::QueryFileCreate => ui_query_file_create(f, app),
        AppMode::QueryFileRename => ui_query_file_rename(f, app),
        AppMode::QueryFileDeleteConfirm => ui_query_file_delete_confirm(f, app),
        AppMode::ModuleCloneSelect => ui_module_clone_select(f, app),
        AppMode::TrackedFolderSelect => ui_tracked_folder_select(f, app),
        AppMode::TrackedFolderAdd => ui_tracked_folder_add(f, app),
        AppMode::TrackedFolderDeleteConfirm => ui_tracked_folder_delete_confirm(f, app),
        AppMode::TrackedFolderCacheClearConfirm => ui_tracked_folder_cache_clear_confirm(f, app),
        AppMode::HelpOverlay => ui_help(f, app),
        AppMode::Normal => ui_normal(f, app),
        AppMode::DocumentCreate => ui_document_create(f, app),
        AppMode::DocumentMetadataEdit => ui_document_metadata_edit(f, app),
    }
}

fn app_background_color() -> Color {
    Color::Rgb(6, 12, 28)
}

fn menu_bar_background_color() -> Color {
    Color::Rgb(160, 150, 110)
}

fn menu_bar_text_color() -> Color {
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
) -> Vec<(&'static str, &'static str)> {
    let mut items = Vec::new();
    match focus {
        Focus::Command => {
            items.push(("Enter", "Execute"));
            items.push(("Tab", "Complete"));
            items.push(("Esc", "Close"));
        }
        Focus::Query => {
            if *edit_mode == EditMode::Insert {
                items.push(("Esc", "Normal"));
                items.push(("^R", "Run"));
                items.push(("^S", "Save"));
            } else {
                items.push(("i", "Insert"));
                items.push(("r", "Run"));
                items.push(("c", "Clone"));
                items.push(("e", "Editor"));
                items.push(("n", "New"));
                if file_list_visible {
                    items.push(("f", "Folders"));
                }
            }
        }
        Focus::Results => {
            items.push(("Enter", "Open"));
            items.push(("Space", "Select"));
            items.push(("n/p", "Page"));
            items.push(("/", "Filter"));
            items.push(("c", "Create"));
            items.push(("^D", "Delete"));
        }
        Focus::Filter => {
            items.push(("Enter", "Apply"));
            items.push(("Esc", "Cancel"));
        }
    }
    items.push(("?", "Help"));
    items
}

fn keybindings_line(focus: &Focus, edit_mode: &EditMode, file_list_visible: bool) -> Line<'static> {
    let items = keybinding_items(focus, edit_mode, file_list_visible);
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

fn styled_menu_bar_items(items: &[String]) -> Line<'static> {
    let white_key = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
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

    let server = app
        .config
        .active_server
        .as_deref()
        .unwrap_or("(no server)");
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
    let server_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let port_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let database_style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);
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
        "Type ':' for a list of commands or '?' for help on any screen"
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

fn syntax_highlighter_for(path: Option<&Path>) -> Option<SyntaxHighlighter> {
    let ext = path?.extension()?.to_str()?;
    let lang = match ext {
        "js" | "sjs" | "mjs" => "js",
        "xqy" => "xml",
        _ => return None,
    };
    SyntaxHighlighter::new("dracula", lang).ok()
}

fn ui_normal(f: &mut Frame, app: &mut App) {
    // Command line is hidden unless focused, filtering, or has content
    let cmd_visible = app.focus == Focus::Command
        || app.focus == Focus::Filter
        || !app.command_input.is_empty()
        || app.uri_filter.is_some()
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

    // Merged top bar: status on left, keybindings on right
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);
    let menu_bar_style = Style::default()
        .bg(menu_bar_background_color())
        .fg(menu_bar_text_color());
    let status = Paragraph::new(app.status_line()).style(menu_bar_style);
    f.render_widget(status, top_chunks[0]);
    let kb_line = keybindings_line(&app.focus, &app.edit_mode, app.file_list_visible);
    let kb_bar = Paragraph::new(kb_line)
        .alignment(Alignment::Right)
        .style(menu_bar_style);
    f.render_widget(kb_bar, top_chunks[1]);

    // Content area: query on top (if visible), results on bottom
    let main_chunks = if app.query_visible {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
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

    if app.query_visible {
        // Query panel: optionally split horizontally for file list
        // edtui manages its own modal state so we don't gate on EditMode::Navigate
        let should_show_file_list =
            app.file_list_visible && (app.use_edtui || app.edit_mode == EditMode::Navigate);
        let (editor_area, file_list_area) = if should_show_file_list {
            let hchunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(30)])
                .split(main_chunks[0]);
            (hchunks[0], Some(hchunks[1]))
        } else {
            (main_chunks[0], None)
        };

        // Query editor (left side, or full width if no file list)
        let border_style = if app.focus == Focus::Query {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
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
                .syntax_highlighter(syntax_highlighter_for(app.active_query_file.as_deref()))
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
            let file_list_style = if app.focus == Focus::Query
                && (app.use_edtui || app.edit_mode == EditMode::Navigate)
            {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let items: Vec<ListItem> = app
                .query_files
                .iter()
                .map(|path| {
                    let is_active = app.active_query_file.as_ref() == Some(path);
                    let name = display_query_path(path, &app.query_root_dir);
                    let marker = if is_active { " *" } else { "" };
                    let text = format!("{}{}", name, marker);
                    let style = if is_active {
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
                        .title("Files [f=folders]")
                        .border_style(file_list_style)
                        .style(Style::default().bg(Color::Rgb(10, 18, 42))),
                )
                .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
                .highlight_symbol("> ");
            f.render_stateful_widget(list, area, &mut app.query_file_list_state);
        }
        f.render_widget(&app.query_editor, editor_area);
    } // end if query_visible

    // Results area
    let results_style = if app.focus == Focus::Results {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

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
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, results_area, &mut app.list_state);
    } else if !app.query_results.is_empty() {
        // Show query results as navigable list with snippets
        let rows: Vec<Row> = app
            .query_results
            .iter()
            .enumerate()
            .map(|(i, result)| {
                // width minus borders(2), index col(4), padding(3)
                let max_width = results_area.width.saturating_sub(9) as usize;
                let snippet = make_snippet(result, max_width);
                let num = format!("{}", i + 1);
                let style = if i % 2 == 0 {
                    Style::default().bg(Color::Rgb(30, 30, 30))
                } else {
                    Style::default()
                };
                Row::new(vec![num, snippet]).style(style)
            })
            .collect();

        let count = app.total_results.unwrap_or(app.query_results.len());
        let title = format!("Query Results ({})", count);
        let mut block = panel_block(title, "[2]").border_style(results_style);
        if let Some(ts) = app.query_results_timestamp {
            block = block.title(Line::from(format_relative_time(ts)).alignment(Alignment::Right));
        }
        let header = Row::new(vec!["#", "Result"]).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        );
        let widths = [Constraint::Length(4), Constraint::Min(0)];
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, results_area, &mut app.query_results_state);
    } else {
        let results = Paragraph::new(app.results_text.as_str())
            .block(panel_block("Results", "[2]").border_style(results_style))
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
        .bg(menu_bar_background_color())
        .fg(menu_bar_text_color());
    f.render_widget(
        Paragraph::new(app.status_line()).style(menu_bar_style),
        top_chunks[0],
    );

    let mut kb_items = vec![
        "Close: Esc/q".to_string(),
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

fn ui_log_viewer(f: &mut Frame, app: &mut App) {
    app.refresh_log_viewer_content();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(f.area());

    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    let total_lines = app.log_view_content.lines().count() as u16;
    let content_height = chunks[1].height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(content_height);
    app.log_view_scroll = app.log_view_scroll.min(max_scroll);
    let pct = if max_scroll == 0 {
        100
    } else {
        ((app.log_view_scroll as f32 / max_scroll as f32) * 100.0) as u16
    };
    app.log_viewer_area_height = chunks[1].height;

    let menu_bar_style = Style::default()
        .bg(menu_bar_background_color())
        .fg(menu_bar_text_color());
    f.render_widget(
        Paragraph::new(app.status_line()).style(menu_bar_style),
        top_chunks[0],
    );

    let mut kb_items = vec![
        "Close: Esc/q".to_string(),
        "j/k: scroll".to_string(),
        "d/u: page".to_string(),
        "gg/G: top/bottom".to_string(),
        "e: edit ext".to_string(),
        "?: help".to_string(),
    ];
    kb_items.push(format!("{}%", pct));
    let kb_bar = Paragraph::new(styled_menu_bar_items(&kb_items))
        .style(menu_bar_style)
        .alignment(Alignment::Right);
    f.render_widget(kb_bar, top_chunks[1]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("Application Log [{}]", app.log_file_path.display()));
    let para = Paragraph::new(app.log_view_content.as_str())
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.log_view_scroll, 0));
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

fn ui_query_file_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .query_files
        .iter()
        .map(|path| {
            let marker = if app.active_query_file.as_ref() == Some(path) {
                " (active)"
            } else {
                ""
            };
            ListItem::new(format!(
                "  {}{}",
                display_query_path(path, &app.query_root_dir),
                marker
            ))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Query File"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.query_file_list_state);
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

fn ui_module_clone_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(90, 90, f.area());
    f.render_widget(Clear, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let filter = Paragraph::new(app.module_clone_filter_input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Clone Filter (type to search module URI)"),
    );
    f.render_widget(filter, rows[0]);

    let items: Vec<ListItem> = app
        .module_clone_filtered_uris
        .iter()
        .map(|uri| ListItem::new(format!("  {}", uri)))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Modules [Tab/j/k move, Enter clone, Esc cancel]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, rows[1], &mut app.module_clone_list_state);

    let cursor_x = rows[0].x + app.module_clone_filter_input.chars().count() as u16 + 1;
    f.set_cursor_position((cursor_x, rows[0].y + 1));
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
        .bg(menu_bar_background_color())
        .fg(menu_bar_text_color());
    f.render_widget(
        Paragraph::new(app.status_line()).style(menu_bar_style),
        top_chunks[0],
    );
    let servers_kb_items = match app.servers_interface_focus {
        ServersInterfaceFocus::Servers => vec![
            "Tab focus".to_string(),
            "Enter activate".to_string(),
            "a add".to_string(),
            "e edit".to_string(),
            "d remove".to_string(),
            "q/Esc close".to_string(),
        ],
        ServersInterfaceFocus::Databases => vec![
            "Tab focus".to_string(),
            "Enter activate".to_string(),
            "r refresh".to_string(),
            "q/Esc close".to_string(),
        ],
        ServersInterfaceFocus::AppServers => vec![
            "Tab focus".to_string(),
            "Enter activate".to_string(),
            "p manage".to_string(),
            "r refresh".to_string(),
            "q/Esc close".to_string(),
        ],
    };
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&servers_kb_items))
            .style(menu_bar_style)
            .alignment(Alignment::Right),
        top_chunks[1],
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(32),
            Constraint::Percentage(28),
            Constraint::Percentage(40),
        ])
        .split(chunks[1]);

    let items: Vec<ListItem> = if app.config.servers.is_empty() {
        vec![ListItem::new("  No servers configured")]
    } else {
        app.server_list
            .iter()
            .map(|s| ListItem::new(format!("  {}", s)))
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Servers [Enter=switch]")
                .border_style(
                    if app.servers_interface_focus == ServersInterfaceFocus::Servers {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, body[0], &mut app.server_list_state);

    let database_items: Vec<ListItem> = if app.database_list.is_empty() {
        vec![ListItem::new("  No databases loaded")]
    } else {
        app.database_list
            .iter()
            .map(|db| {
                let marker = if app.config.active_database.as_deref() == Some(db.as_str()) {
                    " (active)"
                } else {
                    ""
                };
                ListItem::new(format!("  {}{}", db, marker))
            })
            .collect()
    };

    let app_server_items: Vec<ListItem> = if app.app_server_list.is_empty() {
        vec![ListItem::new("  No app servers loaded")]
    } else {
        app.app_server_list
            .iter()
            .map(|app_server| {
                let marker = if app.config.active_app_server.as_deref() == Some(app_server.name.as_str()) {
                    " (active)"
                } else {
                    ""
                };
                let content = app_server
                    .content_database
                    .as_deref()
                    .unwrap_or("(no content db)");
                let modules = app_server
                    .modules_database
                    .as_deref()
                    .unwrap_or("(no modules db)");
                ListItem::new(format!(
                    "  {}{} · :{} · {} · content={} · modules={}",
                    app_server.name,
                    marker,
                    app_server.port,
                    if app_server.secure { "https" } else { "http" },
                    content,
                    modules
                ))
            })
            .collect()
    };

    let db_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body[1]);

    let databases = List::new(database_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Databases [Enter=select, r=refresh]")
                .border_style(
                    if app.servers_interface_focus == ServersInterfaceFocus::Databases {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(databases, db_chunks[0], &mut app.database_list_state);

    let app_servers = List::new(app_server_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("App Servers [Enter=select, p=manage, r=refresh]")
                .border_style(
                    if app.servers_interface_focus == ServersInterfaceFocus::AppServers {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(app_servers, db_chunks[1], &mut app.app_server_list_state);

    let detail = if app.config.servers.is_empty() {
        vec![
            Line::from("No server configurations yet."),
            Line::from(""),
            Line::from("Press 'a' to add your first MarkLogic server."),
        ]
    } else {
        let selected = app.server_list_state.selected().unwrap_or(0);
        if let Some(server) = app.config.servers.get(selected) {
            let active = if app.config.active_server.as_deref() == Some(&server.name) {
                "yes"
            } else {
                "no"
            };
            vec![
                Line::from(vec![
                    Span::styled("Name: ", Style::default().fg(Color::Cyan)),
                    Span::raw(server.name.clone()),
                ]),
                Line::from(vec![
                    Span::styled("Host: ", Style::default().fg(Color::Cyan)),
                    Span::raw(server.host.clone()),
                ]),
                Line::from(vec![
                    Span::styled("Port: ", Style::default().fg(Color::Cyan)),
                    Span::raw(server.port.to_string()),
                ]),
                Line::from(vec![
                    Span::styled("Secure: ", Style::default().fg(Color::Cyan)),
                    Span::raw(if server.secure { "yes" } else { "no" }),
                ]),
                Line::from(vec![
                    Span::styled("Insecure: ", Style::default().fg(Color::Cyan)),
                    Span::raw(if server.insecure { "yes" } else { "no" }),
                ]),
                Line::from(vec![
                    Span::styled("Username: ", Style::default().fg(Color::Cyan)),
                    Span::raw(server.username.clone()),
                ]),
                Line::from(vec![
                    Span::styled("Password: ", Style::default().fg(Color::Cyan)),
                    Span::raw("*".repeat(server.password.len())),
                ]),
                Line::from(vec![
                    Span::styled("Active: ", Style::default().fg(Color::Cyan)),
                    Span::raw(active),
                ]),
                Line::from(vec![
                    Span::styled("Database: ", Style::default().fg(Color::Cyan)),
                    Span::raw(
                        app.config
                            .active_database
                            .as_deref()
                            .unwrap_or("(no database)")
                            .to_string(),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Modules DB: ", Style::default().fg(Color::Cyan)),
                    Span::raw(
                        app.config
                            .active_modules_database
                            .as_deref()
                            .unwrap_or("(no modules db)")
                            .to_string(),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("App Server: ", Style::default().fg(Color::Cyan)),
                    Span::raw(
                        app.config
                            .active_app_server
                            .as_deref()
                            .unwrap_or("(none)")
                            .to_string(),
                    ),
                ]),
                Line::from(""),
                Line::from("Shortcuts"),
                Line::from("  Tab    Switch server/database/app-server focus"),
                Line::from("  Enter  Activate selected server/database/app-server"),
                Line::from("  a      Add a server"),
                Line::from("  e      Edit selected server"),
                Line::from("  d      Remove selected server"),
                Line::from("  p      Manage app servers for selected server"),
                Line::from("  r      Refresh databases/app-servers"),
                Line::from("  q/Esc  Close interface"),
            ]
        } else {
            vec![Line::from("No server selected.")]
        }
    };

    let details = Paragraph::new(detail)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Server Details"),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(details, body[2]);

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
        let style = if i == app.server_form_step {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
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
            .border_style(Style::default().fg(Color::Cyan)),
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

fn ui_app_server_list(f: &mut Frame, app: &App) {
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let server_name = app
        .app_server_list_server_name
        .as_deref()
        .unwrap_or("(unknown)");
    let title = format!("App Servers for '{}' [a add, e edit, d delete, Enter activate, r refresh, Esc back]", server_name);

    let items: Vec<ListItem> = if app.app_server_list.is_empty() {
        vec![ListItem::new("  No app servers configured")]
    } else {
        app.app_server_list
            .iter()
            .map(|ep| {
                let marker = if app.config.active_app_server.as_deref() == Some(ep.name.as_str()) {
                    "* "
                } else {
                    "  "
                };
                let content = format!(
                    "{}{} | port: {} | {} | db: {} | modules: {}",
                    marker,
                    ep.name,
                    ep.port,
                    if ep.secure { "https" } else { "http" },
                    ep.content_database.as_deref().unwrap_or("-"),
                    ep.modules_database.as_deref().unwrap_or("-"),
                );
                ListItem::new(content)
            })
            .collect()
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    f.render_stateful_widget(list, chunks[0], &mut app.app_server_list_state.clone());

    let help = Paragraph::new("Tab: focus | a: add | e: edit | d: delete | Enter: activate | r: refresh | Esc: back")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[1]);
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
        let style = if i == app.app_server_form_step {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
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
            .border_style(Style::default().fg(Color::Cyan)),
        area,
    );

    let field_area = chunks[app.app_server_form_step];
    let cursor_x =
        field_area.x + app.app_server_form_fields[app.app_server_form_step].chars().count() as u16 + 1;
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
        let style = if i == app.doc_form_step {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
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
            .border_style(Style::default().fg(Color::Cyan)),
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
        let style = if i == app.doc_form_step {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
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
            .border_style(Style::default().fg(Color::Cyan)),
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

fn help_text_for_focus(focus: &Focus, edit_mode: &EditMode, mode: &AppMode) -> Vec<&'static str> {
    if matches!(
        mode,
        AppMode::Interface(AppInterface::Servers)
            | AppMode::ServerForm
            | AppMode::ServerDeleteConfirm
    ) {
        return vec![
            "",
            "  Servers Interface",
            "    Tab        Switch between servers/databases/app-servers",
            "    Enter      Activate selected server/database/app-server",
            "    a          Add server (server focus)",
            "    e          Edit selected server (server focus)",
            "    d          Remove selected server (server focus)",
            "    r          Refresh databases/app-servers",
            "    j/k        Move selection down/up",
            "    g/G        Go to top/bottom",
            "    q / Esc    Close servers interface",
            "",
            "  Server Form",
            "    Tab        Next field",
            "    Shift+Tab  Previous field",
            "    Enter      Save server",
            "    Esc        Cancel",
            "",
            "  Press ? or Esc to close this help",
        ];
    }

    if *mode == AppMode::FullScreenView {
        return vec![
            "",
            "  Document View",
            "    Esc / q    Close document view",
            "    e          Edit in external editor",
            "    m          Edit document (full-screen form)",
            "    j / k      Scroll down/up",
            "    d / u      Page down/up",
            "    g / G      Top/bottom",
            "    Space      Page down",
            "    PageUp     Page up",
            "",
            "  Press ? or Esc to close this help",
        ];
    }

    if *mode == AppMode::LogViewer {
        return vec![
            "",
            "  Log Viewer",
            "    Esc / q    Close log viewer",
            "    e          Open log in external editor",
            "    j / k      Scroll down/up",
            "    d / u      Page down/up",
            "    g / G      Top/bottom",
            "    Space      Page down",
            "    PageUp     Page up",
            "",
            "  Press ? or Esc to close this help",
        ];
    }

    if matches!(
        mode,
        AppMode::DocumentCreate | AppMode::DocumentMetadataEdit
    ) {
        return vec![
            "",
            "  Document Form",
            "    Tab        Next field",
            "    Backspace  Delete character",
            "    Enter      Save document",
            "    Esc        Cancel",
            "",
            "  Fields",
            "    URI        Document URI (required)",
            "    Collections  Comma-separated list",
            "    Quality    Document quality (integer)",
            "    Content    Document body",
            "",
            "  Press ? or Esc to close this help",
        ];
    }

    let mut lines = vec![
        "",
        "  Global",
        "    ?          Show/hide this help",
        "    F2         Open application logs",
        "    :          Open command input",
        "    1          Focus query panel",
        "    2          Focus results panel",
        "    3          Focus command panel",
        "    \\          Toggle file list",
        "    Tab        Cycle focus forward",
        "    Shift+Tab  Cycle focus backward",
        "",
    ];

    match focus {
        Focus::Command => {
            lines.extend_from_slice(&[
                "  Command Panel",
                "    Enter      Execute command",
                "    Tab        Accept autocomplete / cycle panels",
                "    Up/Down    Navigate autocomplete",
                "    Esc        Close command line",
                "",
            ]);
        }
        Focus::Query => {
            if *edit_mode == EditMode::Insert {
                lines.extend_from_slice(&[
                    "  Query Panel (Insert Mode)",
                    "    Esc        Return to normal mode",
                    "    Ctrl+T     Cycle focus",
                    "    Ctrl+R     Run query",
                    "    Ctrl+S     Save query file",
                    "    Ctrl+O     Switch query file",
                    "",
                ]);
            } else {
                lines.extend_from_slice(&[
                    "  Query Panel (Normal Mode)",
                    "    i          Enter insert mode (editor)",
                    "    j/k        Navigate + auto-load file",
                    "    Enter      Load selected file",
                    "    f          Open tracked folders",
                    "    c          Clone module",
                    "    n          New query file",
                    "    m          Rename selected file",
                    "    Delete     Delete selected file",
                    "    r / F5     Run query",
                    "    e          Open in external editor",
                    "    \\          Toggle file list",
                    "    g/G        Go to top/bottom of file list",
                    "",
                ]);
            }
        }
        Focus::Results => {
            lines.extend_from_slice(&[
                "  Results Panel",
                "    Enter      Open selected document",
                "    j/k        Navigate up/down",
                "    d          Page down",
                "    u          Page up",
                "    n / p      Next/previous page",
                "    Space      Toggle selection",
                "    /          Filter results",
                "    c          Create new document",
                "    Ctrl+D     Delete selected",
                "    g          Go to top (double-tap)",
                "    G          Go to bottom",
                "",
            ]);
        }
        Focus::Filter => {
            lines.extend_from_slice(&[
                "  Filter Panel",
                "    Enter      Apply filter",
                "    Esc        Cancel filter",
                "",
            ]);
        }
    }

    lines.push("  Press ? or Esc to close this help");
    lines.into()
}

fn ui_help(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 70, f.area());
    f.render_widget(Clear, area);

    let help_lines = help_text_for_focus(&app.focus, &app.edit_mode, &app.mode);
    let text = help_lines.join("\n");

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Keybindings")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(text)
        .block(block)
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
