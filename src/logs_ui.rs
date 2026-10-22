use crate::logs::{LogViewer, format_bytes};
use crate::ui::{menu_bar_background_color, styled_menu_bar_items};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph},
};

const SIDEBAR_WIDTH: u16 = 34;

fn truncate_sidebar_label(label: String, max_width: usize) -> String {
    let width = label.chars().count();
    if width <= max_width {
        return label;
    }
    if max_width <= 1 {
        return "…".chars().take(max_width).collect();
    }
    format!("{}…", label.chars().take(max_width - 1).collect::<String>())
}

pub(crate) fn render(f: &mut Frame, logs: &mut LogViewer) {
    let details_height = if logs.combined && !logs.options.chrome {
        0
    } else {
        2
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(details_height),
            Constraint::Min(3),
            Constraint::Length(details_height),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    let heading = vec![
        format!(
            "Logs: {}/{}/{}{}",
            if logs.combined { "merged" } else { "file" },
            if logs.following { "live" } else { "paused" },
            logs.scope_label(),
            if logs.busy { " (sync)" } else { "" },
        ),
        "m: combine".into(),
        "\\: files".into(),
        "f: follow".into(),
        if logs.combined {
            "v: display".into()
        } else {
            "Enter: fetch".into()
        },
        "p: ports".into(),
        "r: discover".into(),
        "C: restart".into(),
        "F: find".into(),
        "/: search".into(),
        "?: help".into(),
        "q: close".into(),
    ];
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&heading))
            .style(Style::default().bg(menu_bar_background_color())),
        chunks[0],
    );
    let stats = format!(
        "Remote: {} | Cached: {} | HTTP bodies: {} | Last operation: {}\nServer read estimate: {} | Cluster payload estimate: {} | Host problems: {}",
        format_bytes(logs.remote_total()),
        format_bytes(logs.cached_total()),
        format_bytes(logs.body_bytes),
        format_bytes(logs.last_bytes),
        format_bytes(logs.read_bytes),
        format_bytes(logs.cluster_bytes),
        logs.errors.len()
    );
    f.render_widget(Paragraph::new(stats), chunks[1]);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if logs.sidebar_visible {
            [Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)]
        } else {
            [Constraint::Length(0), Constraint::Percentage(100)]
        })
        .split(chunks[2]);
    let host_legend = logs.host_legend_lines();
    let mut legend_lines = vec!["size: last known".to_owned()];
    legend_lines.extend(host_legend);
    let legend = legend_lines.join("\n");
    let sidebar = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(legend_lines.len().max(1) as u16),
        ])
        .split(panes[0]);
    // Borders take two cells and an always-reserved selection glyph takes
    // two more. Keeping that column reserved prevents the size from shifting
    // between the selected and unselected rows.
    let item_width = usize::from(sidebar[0].width.saturating_sub(4));
    let visible_files = logs.visible_file_indices();
    let window_files = logs.window_file_count();
    let items: Vec<ListItem> = visible_files
        .iter()
        .filter_map(|index| logs.files.get(*index))
        .map(|file| {
            let tracked = logs.is_subscribed(file);
            let file_style = if tracked {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if file.present {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let size = format_bytes(file.size);
            let label = truncate_sidebar_label(
                format!(
                    "{} {}. {}",
                    logs.port_filter_marker(file),
                    logs.host_alias(&file.host),
                    logs.compact_file_label(file),
                ),
                item_width.saturating_sub(size.chars().count() + 1),
            );
            let padding = " ".repeat(
                item_width
                    .saturating_sub(label.chars().count() + size.chars().count())
                    .max(1),
            );
            ListItem::new(Line::from(vec![
                Span::styled(label, file_style),
                Span::styled(
                    format!("{padding}{size}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]))
        })
        .collect();
    let files = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    "Logs ({}{}; {}; ports: {}){}",
                    visible_files.len(),
                    if logs.file_filter.is_empty() {
                        String::new()
                    } else {
                        format!("/{window_files}")
                    },
                    logs.scope_label(),
                    logs.port_filter_summary(),
                    if logs.file_filter.is_empty() {
                        String::new()
                    } else {
                        format!("; filter: {}", logs.file_filter)
                    },
                ))
                .border_style(Style::default().fg(if logs.files_focus {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("› ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut selection = ListState::default();
    if !visible_files.is_empty() {
        selection.select(logs.selected_visible_index());
    }
    if logs.sidebar_visible {
        f.render_stateful_widget(files, sidebar[0], &mut selection);
        f.render_widget(
            Paragraph::new(legend).style(Style::default().fg(Color::DarkGray)),
            sidebar[1],
        );
    }
    let height = panes[1].height.saturating_sub(2) as usize;
    logs.resize_content(usize::from(panes[1].width.saturating_sub(2)));
    logs.view_height = height;
    logs.scroll = logs.scroll.min(logs.matches.len().saturating_sub(height));
    let content: Vec<Line> = if logs.lines.is_empty() {
        vec![
            Line::from("Cached tracked logs are shown here; r discovers logs on the cluster."),
            Line::from(
                "m merges tracked streams; \\ hides files; v changes display; D widens history.",
            ),
            Line::from(
                "Only records newer than the saved cursor are requested, so returning later fills the gap.",
            ),
        ]
    } else {
        logs.matches
            .iter()
            .skip(logs.scroll)
            .take(height)
            .map(|&index| {
                if logs.combined
                    && let Some(row) = logs.rows.get(index)
                {
                    return row.line.clone();
                }
                let text = &logs.lines[index];
                let color = if text.contains("Error:")
                    || text.contains("Fatal:")
                    || text.contains("Alert:")
                {
                    Color::Red
                } else if text.contains("Warning:") {
                    Color::Yellow
                } else {
                    Color::White
                };
                Line::from(Span::styled(
                    format!("{:>7}  {}", index + 1, text),
                    Style::default().fg(color),
                ))
            })
            .collect()
    };
    let title = logs
        .selected_file()
        .map(|file| format!("{} / {}", file.host, file.name))
        .unwrap_or_else(|| "Cached content".into());
    let title = if logs.combined {
        format!(
            "Combined {} · chronological · {}",
            if logs.all_cached {
                "cached logs"
            } else {
                "tracked logs"
            },
            if logs.options.raw { "raw" } else { "formatted" }
        )
    } else if logs.all_cached {
        "All cached logs (grouped by host/file)".into()
    } else {
        title
    };
    let content = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(if !logs.files_focus {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })),
        )
        .scroll((
            0,
            if logs.combined && logs.options.wrap {
                0
            } else {
                logs.horizontal
            },
        ));
    f.render_widget(content, panes[1]);
    let details = if logs.combined {
        format!(
            "{} · ports: {} · {} · {}\nCache: {}",
            logs.scope_label(),
            logs.port_filter_summary(),
            if logs.following {
                "following (scroll up to review; G/f returns to tail)"
            } else {
                "collection paused"
            },
            if logs.all_cached {
                "all cached sources"
            } else {
                "tracked sources only"
            },
            logs.cache_root.display()
        )
    } else {
        format!(
            "{}\nCache: {}",
            logs.selected_status(),
            logs.local_path()
                .unwrap_or_else(|| logs.cache_root.clone())
                .display()
        )
    };
    f.render_widget(
        Paragraph::new(details).style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
    let mut search_help = if logs.files_focus {
        vec![format!(
            "Files: {} · {} matching / {} visible",
            logs.file_filter,
            visible_files.len(),
            logs.files.iter().filter(|file| file.size > 0).count(),
        )]
    } else {
        vec![format!(
            "Search: {} · {} matching / {} lines{}",
            logs.search,
            logs.matches.len(),
            logs.lines.len(),
            if logs.combined {
                format!(" · {}", logs.options.search_mode.label())
            } else {
                String::new()
            },
        )]
    };
    if logs.file_filter_editing {
        search_help.extend(["Enter: apply".into(), "Esc: clear".into()]);
    } else if logs.search_editing {
        search_help.extend(["Enter: apply".into(), "Esc: cancel and clear".into()]);
    } else if logs.files_focus {
        search_help.extend(["F: find".into(), "o: track".into(), "c: clear".into()]);
    } else {
        search_help.extend(["/: edit".into(), "c: clear".into()]);
        if logs.combined {
            search_help.push("n/N: matches".into());
        }
    }
    if logs.time_editing {
        search_help = vec![
            format!("Seek: {}", logs.time_input),
            "Enter: go".into(),
            "Esc: cancel".into(),
        ];
    }
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&search_help))
            .style(Style::default().bg(menu_bar_background_color())),
        chunks[4],
    );
    let message = if logs.errors.is_empty() {
        logs.message.clone()
    } else {
        format!(
            "{} host problem(s): {}",
            logs.errors.len(),
            logs.errors
                .iter()
                .map(|(host, error)| format!("{host}: {error}"))
                .collect::<Vec<_>>()
                .join(" | ")
        )
    };
    f.render_widget(
        Paragraph::new(message).style(Style::default().fg(Color::Cyan)),
        chunks[5],
    );
    if logs.file_filter_editing || logs.search_editing || logs.time_editing {
        let label = if logs.time_editing {
            "Seek: "
        } else if logs.file_filter_editing {
            "Files: "
        } else {
            "Search: "
        };
        let text = if logs.time_editing {
            &logs.time_input
        } else {
            if logs.file_filter_editing {
                &logs.file_filter
            } else {
                &logs.search
            }
        };
        let column = (label.chars().count() + text.chars().count())
            .min(usize::from(chunks[4].width.saturating_sub(1))) as u16;
        f.set_cursor_position((chunks[4].x + column, chunks[4].y));
    }
    if logs.options_open {
        render_options(f, logs);
    }
}

fn render_options(f: &mut Frame, logs: &LogViewer) {
    let area = f.area();
    let width = area.width.saturating_sub(4).min(64);
    let height = area.height.saturating_sub(2).min(15);
    let modal = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, modal);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(modal);
    let list = List::new(
        logs.option_items()
            .into_iter()
            .map(ListItem::new)
            .collect::<Vec<_>>(),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(if logs.options_ports {
                "Ports · default → include → exclude"
            } else {
                "Combined log display"
            })
            .border_style(Style::default().fg(Color::Cyan)),
    )
    .highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(logs.option_selected));
    f.render_stateful_widget(list, rows[0], &mut state);
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&[
            "j/k: select".into(),
            "Enter/Space: change".into(),
            "Esc: close".into(),
        ]))
        .style(Style::default().bg(menu_bar_background_color())),
        rows[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn combined_logs_fill_the_width_when_sidebar_and_details_are_hidden() {
        let mut logs = LogViewer::default();
        logs.combined = true;
        logs.sidebar_visible = false;
        logs.files_focus = false;
        logs.options.chrome = false;
        let line = Line::from(Span::styled(
            "ERR  9060 12:30:01.123 EE: failure",
            Style::default().fg(Color::LightRed),
        ));
        logs.lines = vec![line.to_string()];
        logs.rows = vec![crate::logs::ViewRow {
            line,
            timestamp: None,
            error: true,
            start: true,
            search_match: true,
        }];
        logs.matches = vec![0];
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| render(f, &mut logs)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("ERR  9060"));
        assert!(!screen.contains("Host / File"));
        assert!(!screen.contains("HTTP bodies"));
        assert_eq!(terminal.backend().buffer()[(1, 2)].fg, Color::LightRed);
        for (width, height) in [(20, 5), (1, 1)] {
            logs.options_open = true;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &mut logs)).unwrap();
        }
    }

    #[test]
    fn log_view_handles_small_terminals_and_scrolls_beyond_u16_rows() {
        let mut logs = LogViewer::default();
        logs.lines = (0..70_000).map(|i| format!("line-{i}")).collect();
        logs.matches = (0..70_000).collect();
        logs.scroll = 69_980;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut logs)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("line-69980"));
        assert!(screen.contains("HTTP bodies"));
        assert!(logs.scroll > u16::MAX as usize);
        for (width, height) in [(60, 12), (20, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &mut logs)).unwrap();
        }
    }
}
