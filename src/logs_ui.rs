use crate::logs::{
    CacheFreshness, LogMouseTarget, LogViewer, TrackingMode, format_bytes, presentation::Patterns,
};
use crate::ui::{active_border_style, menu_bar_background_color, styled_menu_bar_items};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};

const GRID_PORT_WIDTH: u16 = 7;
const GRID_DAY_WIDTH: u16 = 8;

fn with_line_number(mut line: Line<'static>, number: usize) -> Line<'static> {
    line.spans.insert(
        0,
        Span::styled(format!("{number:>6} "), Style::default().fg(Color::White)),
    );
    line
}

fn truncation_notice(logs: &LogViewer) -> Option<Line<'static>> {
    if logs.window.start.is_none() && logs.window.end.is_none() {
        return None;
    }
    let background = Color::Yellow;
    Some(Line::styled(
        format!("Window: {} · Ctrl+T edit", logs.window_label()),
        Style::default().fg(Color::Black).bg(background),
    ))
}

pub(crate) fn render(f: &mut Frame, logs: &mut LogViewer, terminal_focused: bool) {
    logs.mouse_regions.clear();
    if let Some(status) = logs.cache_loading_status() {
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());
        f.render_widget(
            Paragraph::new(format!(
                "{} · Logs · loading local history",
                logs.environment_indicator
            ))
            .style(Style::default().bg(menu_bar_background_color(terminal_focused))),
            areas[0],
        );
        f.render_widget(
            Paragraph::new(vec![
                Line::default(),
                Line::from(status),
                Line::default(),
                Line::from("Cached files will appear when loading finishes."),
            ])
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" Logs ")
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            areas[1],
        );
        f.render_widget(Paragraph::new("q: close · x: cancel loading"), areas[2]);
        return;
    }
    let filter_active = logs.content_filter_active()
        || !logs.search.is_empty()
            && (!logs.combined
                || logs.options.search_mode == crate::logs::presentation::SearchMode::Filter);
    let file_search = (!logs.combined && !logs.search.is_empty())
        .then(|| Patterns::new(&format!("/{}/i", regex::escape(&logs.search))));
    let mut chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area())
        .to_vec();
    let progress = logs.download_progress();
    let progress_area = if progress.is_some() && chunks[1].height > 1 {
        chunks[1].height -= 1;
        Some(Rect::new(
            chunks[1].x,
            chunks[1].bottom(),
            chunks[1].width,
            1,
        ))
    } else {
        None
    };
    let mut heading = vec![
        format!(
            "Logs: {}/{}/collection {}{}",
            if logs.has_log_selection() {
                "selection"
            } else if logs.combined {
                "merged"
            } else {
                "file"
            },
            if logs.following {
                "following"
            } else {
                "reviewing"
            },
            logs.scope_label(),
            if logs.busy { " (sync)" } else { "" },
        ),
        "Q: downloads".into(),
        "^.: previous".into(),
        if logs.combined {
            "m: unmerge".into()
        } else {
            "m: merge".into()
        },
        "\\: files".into(),
        if logs.files_focus {
            "Enter: open".into()
        } else if logs.combined {
            "v: display".into()
        } else {
            "d/u: page".into()
        },
        if logs.files_focus {
            "V: row".into()
        } else {
            "C: clear cache".into()
        },
        "?: help".into(),
        "q: close".into(),
    ];
    if logs.files_focus {
        heading.extend([
            "C: column".into(),
            "p: ports".into(),
            "r: day keys".into(),
            "F or /: filter".into(),
        ]);
    } else {
        heading.extend([
            "f: follow".into(),
            "t/T: truncate".into(),
            "U: undo".into(),
            "^T: window".into(),
            "/: search".into(),
            if logs.show_line_numbers {
                "#: hide lines".into()
            } else {
                "#: line numbers".into()
            },
        ]);
    }
    if logs.hosts_focus {
        heading.truncate(1);
        heading.extend([
            "Tab: next pane".into(),
            "↑/↓: select".into(),
            "Space: toggle".into(),
            "i: only".into(),
            "a: all".into(),
            "?: help".into(),
            "q: close".into(),
        ]);
    } else {
        heading.insert(1, "Tab: panes".into());
    }
    if logs.merged_port_view() && !logs.hosts_focus {
        heading.push("D: download both".into());
    }
    if !logs.environment_indicator.is_empty() {
        heading.insert(0, logs.environment_indicator.clone());
    }
    if !logs.errors.is_empty() {
        heading.push("!: errors".into());
    }
    if !logs.files_focus && !logs.combined && !logs.all_cached && logs.host_views.len() > 1 {
        heading.push(if logs.host_pane_maximized {
            "z: restore panes".into()
        } else {
            "z: maximize pane".into()
        });
    }
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&heading))
            .style(Style::default().bg(menu_bar_background_color(terminal_focused))),
        chunks[0],
    );
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if logs.sidebar_visible {
            let width = if logs.files_focus {
                (chunks[1].width * 60 / 100).max(1)
            } else {
                (GRID_PORT_WIDTH + GRID_DAY_WIDTH + 2).min(chunks[1].width / 2)
            };
            [Constraint::Length(width), Constraint::Min(1)]
        } else {
            [Constraint::Length(0), Constraint::Percentage(100)]
        })
        .split(chunks[1]);
    let mut control_lines: Vec<Line> = vec![
        Line::from(format!(
            "Ports {}/{} · {}",
            logs.visible_port_rows().len(),
            logs.available_port_count(),
            logs.port_filter_summary()
        )),
        Line::from("V/C: select · t: track · o: mode · p: ports"),
        Line::from("d: download  u: untrack"),
        Line::from("↓ on-demand  ↻ live  ◐ partial  · untracked"),
        Line::from("Green fresh · Yellow stale · Blue missing · Gray empty"),
    ];
    let selected_group = logs.selected_visible_index();
    let host_count = logs.host_names().len();
    // Reserve space for the file grid while keeping the host list scrollable.
    let host_height = (host_count.saturating_add(2).max(3).min(u16::MAX as usize) as u16)
        .min((panes[0].height / 3).max(3))
        .min(panes[0].height.saturating_sub(5));
    control_lines.truncate(usize::from(panes[0].height.saturating_sub(host_height + 5)));
    let host_legend_capacity = host_count.min(usize::from(host_height.saturating_sub(2)));
    let sidebar = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(host_height),
            Constraint::Min(5),
            Constraint::Length(control_lines.len() as u16),
        ])
        .split(panes[0]);
    let port_rows = logs.visible_port_rows();
    let visible_files: Vec<usize> = port_rows
        .iter()
        .flat_map(|(_, cells)| cells.iter().flatten().copied())
        .collect();
    let status = logs.sidebar_status(chrono::Utc::now());
    let group_summaries = &status.summaries;
    if let (Some(file), Some((port, _))) = (
        logs.selected_file(),
        selected_group.and_then(|row| port_rows.get(row)),
    ) {
        let kind = ["Err", "Acc", "Req"][logs.selected_kind_column()];
        if let Some(line) = control_lines.get_mut(2) {
            if logs.merged_port_view() && logs.selected_display_column() == 0 {
                *line = Line::from(format!("Selected {port} Mrg · D: download both"));
            } else if let Some((_, size, age)) = group_summaries.get(file.name.as_str()) {
                *line = Line::from(format!(
                    "Selected {port} {kind}: {} · last update {age}",
                    format_bytes(*size)
                ));
            }
        }
    }
    // Keep the selected day on screen as the user moves into older rotations.
    let day_capacity =
        usize::from(panes[0].width.saturating_sub(2 + GRID_PORT_WIDTH) / GRID_DAY_WIDTH).max(1);
    let first_day = if logs.files_focus {
        logs.discovery_days_ago
            .saturating_sub(day_capacity.saturating_sub(1) as u32)
    } else {
        logs.discovery_days_ago
    };
    logs.grid_first_day = first_day;
    logs.grid_day_count = day_capacity;
    let days: Vec<u32> = (0..day_capacity)
        .map(|n| first_day.saturating_add(n as u32))
        .collect();
    let day_rows: Vec<_> = days
        .iter()
        .map(|day| logs.port_rows_for_day(*day))
        .collect();
    let mut ports: Vec<_> = day_rows
        .iter()
        .flat_map(|rows| rows.iter().map(|(port, _)| port.clone()))
        .collect();
    if logs.empty_grid_cell()
        && let Some(port) = logs.selected_port()
    {
        ports.push(port.to_owned());
    }
    ports.sort_by(|a, b| LogViewer::port_label_sort_key(a).cmp(&LogViewer::port_label_sort_key(b)));
    ports.dedup();
    let grid_selected = logs
        .selected_port()
        .and_then(|port| ports.iter().position(|p| p == port));
    let freshness = &status.freshness;
    let modes = &status.modes;
    let items: Vec<ListItem> = ports
        .iter()
        .enumerate()
        .map(|(row, port)| {
            let row_selected = grid_selected == Some(row);
            let row_bg = if row_selected {
                Color::Rgb(34, 43, 58)
            } else {
                Color::Reset
            };
            let mut spans = vec![Span::styled(
                format!("{:<7}", if port == "----" { "ROOT" } else { port }),
                Style::default().fg(Color::White).bg(row_bg),
            )];
            for (column, day) in days.iter().enumerate() {
                let selected_day = *day == logs.discovery_days_ago;
                let bg = if row_selected && selected_day {
                    Color::Rgb(70, 80, 100)
                } else if selected_day {
                    Color::Rgb(30, 39, 52)
                } else {
                    row_bg
                };
                let cells = day_rows[column]
                    .iter()
                    .find(|(p, _)| p == port)
                    .map(|(_, cells)| *cells)
                    .unwrap_or_default();
                spans.push(Span::styled(" ", Style::default().bg(bg)));
                for (kind, index) in cells.iter().enumerate() {
                    let (icon, fg) = index
                        .and_then(|i| logs.files.get(i))
                        .map(|file| {
                            let mode = modes
                                .get(file.name.as_str())
                                .copied()
                                .unwrap_or(TrackingMode::Untracked);
                            let color = match freshness.get(file.name.as_str()) {
                                Some(CacheFreshness::Fresh) => Color::Green,
                                Some(CacheFreshness::Stale) => Color::Yellow,
                                Some(CacheFreshness::NoData) => Color::Rgb(45, 45, 45),
                                Some(CacheFreshness::Missing) | None => Color::LightBlue,
                            };
                            let icon = if matches!(
                                freshness.get(file.name.as_str()),
                                Some(CacheFreshness::NoData)
                            ) {
                                "·"
                            } else {
                                match mode {
                                    TrackingMode::Live => "↻",
                                    TrackingMode::OnDemand => "↓",
                                    TrackingMode::Partial => "◐",
                                    TrackingMode::Untracked => "·",
                                }
                            };
                            (icon, color)
                        })
                        .unwrap_or((" ", Color::DarkGray));
                    let group_selected = index
                        .and_then(|index| logs.files.get(index))
                        .is_some_and(|file| logs.is_log_selected(file));
                    let cursor =
                        row_selected && selected_day && kind == logs.selected_kind_column();
                    let style = if group_selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Magenta)
                            .add_modifier(if cursor {
                                Modifier::BOLD | Modifier::UNDERLINED
                            } else {
                                Modifier::BOLD
                            })
                    } else if cursor {
                        Style::default()
                            .fg(Color::Black)
                            .bg(fg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(fg).bg(bg)
                    };
                    spans.push(Span::styled(icon, style));
                    spans.push(Span::styled(" ", Style::default().bg(bg)));
                }
                spans.push(Span::styled(" ", Style::default().bg(bg)));
            }
            spans.push(Span::styled(
                " ".repeat(usize::from(panes[0].width.saturating_sub(
                    2 + GRID_PORT_WIDTH + GRID_DAY_WIDTH * day_capacity as u16,
                ))),
                Style::default().bg(row_bg),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let hosts = logs.host_names();
    let count = hosts.len();
    let capacity = host_legend_capacity;
    let mut host_offset = logs.host_detail_offset.min(count.saturating_sub(capacity));
    logs.host_detail_count = count;
    logs.host_detail_capacity = capacity;
    logs.host_detail_offset = host_offset;
    logs.host_selected = logs.host_selected.min(count.saturating_sub(1));
    if logs.hosts_focus && capacity > 0 {
        if logs.host_selected < host_offset {
            host_offset = logs.host_selected;
        } else if logs.host_selected >= host_offset + capacity {
            host_offset = logs.host_selected + 1 - capacity;
        }
        logs.host_detail_offset = host_offset;
    }
    let details = logs.selected_host_details();
    let merged_details = logs.merged_host_details();
    let now = chrono::Utc::now();
    let mut host_lines = Vec::new();
    if count == 0 {
        host_lines.push(Line::from("No hosts available."));
    }
    let aliases = logs.host_aliases();
    for (index, host) in hosts.iter().enumerate().skip(host_offset).take(capacity) {
        let alias = aliases.get(host).map(String::as_str).unwrap_or("h1");
        let enabled = logs.host_enabled(host);
        let summary = if logs.merged_port_view() && logs.selected_display_column() == 0 {
            merged_details
                .iter()
                .find(|(_, name, ..)| *name == host)
                .map(|(_, _, error, access, error_result, access_result)| {
                    let size = |file: Option<&crate::logs::LogFile>| match file {
                        Some(file) if file.present => format_bytes(file.size),
                        Some(_) => "offline".into(),
                        None => "n/a".into(),
                    };
                    let marker = |result| match result {
                        Some(true) => "✓",
                        Some(false) => "!",
                        None => "",
                    };
                    format!(
                        " E:{}{} A:{}{}",
                        size(*error),
                        marker(*error_result),
                        size(*access),
                        marker(*access_result)
                    )
                })
                .unwrap_or_default()
        } else {
            details
                .iter()
                .find(|(_, name, ..)| *name == host)
                .map(|(_, _, file, result)| {
                    let size = match file {
                        Some(file) if file.present => format_bytes(file.size),
                        Some(_) => "unavailable".into(),
                        None => "not listed".into(),
                    };
                    let age = file
                        .map(|file| logs.cache_age(file, now))
                        .unwrap_or_else(|| "n/a".into());
                    let marker = match result {
                        Some(true) => " ok",
                        Some(false) => " failed",
                        None => "",
                    };
                    format!(" {size}  {age}{marker}")
                })
                .unwrap_or_default()
        };
        let style = Style::default()
            .fg(if enabled {
                Color::White
            } else {
                Color::DarkGray
            })
            .bg(if logs.hosts_focus && logs.host_selected == index {
                Color::Rgb(70, 80, 100)
            } else {
                Color::Rgb(24, 32, 46)
            });
        host_lines.push(Line::styled(
            format!(
                "[{}] {alias}={host}{summary}",
                if enabled { "x" } else { " " }
            ),
            style,
        ));
    }
    let hosts_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " Hosts {}/{} · ,/.: scroll ",
            hosts.iter().filter(|host| logs.host_enabled(host)).count(),
            count,
        ))
        .border_style(active_border_style(
            logs.hosts_focus,
            terminal_focused,
            Color::Yellow,
        ));
    let hosts_inner = hosts_block.inner(sidebar[0]);
    let mut day_header = vec![Span::styled(
        "PORT   ",
        Style::default().fg(Color::White).bg(Color::DarkGray),
    )];
    let mut kind_header = vec![Span::styled(
        "       ",
        Style::default().bg(Color::DarkGray),
    )];
    for day in &days {
        let style = if *day == logs.discovery_days_ago {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        };
        let label = if *day == 0 {
            "TODAY".to_owned()
        } else {
            format!("DAY {day}")
        };
        day_header.push(Span::styled(format!("{label:<8}"), style));
        kind_header.push(Span::styled(" E A R  ", style));
    }
    let files_header = vec![Line::from(day_header), Line::from(kind_header)];
    let files_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " Files · CURRENT DAY {} ",
            logs.discovery_day() + chrono::Duration::days(i64::from(logs.discovery_days_ago))
        ))
        .border_style(active_border_style(
            logs.files_focus && !logs.hosts_focus,
            terminal_focused,
            Color::Yellow,
        ));
    let inner = files_block.inner(sidebar[1]);
    let header_height = inner.height.min(2);
    let header_area = Rect::new(inner.x, inner.y, inner.width, header_height);
    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(header_height),
        inner.width,
        inner.height.saturating_sub(header_height),
    );
    let files = List::new(items);
    let list_height = usize::from(list_area.height);
    let offset = grid_selected.map_or(0, |selected| (selected + 1).saturating_sub(list_height));
    let mut selection = ListState::default().with_offset(offset);
    selection.select(grid_selected);
    if logs.sidebar_visible {
        f.render_widget(hosts_block, sidebar[0]);
        f.render_widget(Paragraph::new(host_lines), hosts_inner);
        logs.mouse_regions.push((sidebar[0], LogMouseTarget::Hosts));
        for (line, index) in (host_offset..host_offset + capacity).enumerate() {
            let row = hosts_inner.y.saturating_add(line as u16);
            if row < hosts_inner.bottom() {
                logs.mouse_regions.push((
                    Rect::new(hosts_inner.x, row, hosts_inner.width, 1),
                    LogMouseTarget::HostChoice(index),
                ));
            }
        }
        f.render_widget(files_block, sidebar[1]);
        f.render_widget(
            Paragraph::new(files_header).style(Style::default().bg(Color::DarkGray)),
            header_area,
        );
        f.render_stateful_widget(files, list_area, &mut selection);
        logs.mouse_regions.push((
            sidebar[1],
            LogMouseTarget::Files {
                offset: selection.offset(),
                selected: grid_selected,
                details: 0,
            },
        ));
        f.render_widget(Paragraph::new(control_lines), sidebar[2]);
    }
    logs.resize_content(usize::from(panes[1].width.saturating_sub(2)));
    // Keep the histogram outside the scrolling content and share its time axis
    // across host panes. Very short terminals retain room for log messages.
    let host_panes = if !logs.preview_pending()
        && !logs.combined
        && !logs.all_cached
        && !logs.host_pane_maximized
        && !visible_files.is_empty()
    {
        logs.host_views.len().max(1)
    } else {
        1
    };
    let minimum_log_height = (host_panes * 3).max(4);
    let individual_charts = !logs.preview_pending()
        && !logs.combined
        && !logs.all_cached
        && !logs.host_views.is_empty()
        && !visible_files.is_empty();
    let content_area = if !individual_charts
        && usize::from(panes[1].height) >= 4 + minimum_log_height
        && panes[1].width >= 4
    {
        let sections =
            Layout::vertical([Constraint::Length(4), Constraint::Min(3)]).split(panes[1]);
        if !logs.preview_pending() {
            render_activity_graph(f, logs, sections[0], None, true);
        }
        sections[1]
    } else {
        panes[1]
    };
    let lead = logs.leading_host_index().unwrap_or(0);
    let host_areas = if !logs.preview_pending()
        && !logs.combined
        && !logs.all_cached
        && !logs.host_views.is_empty()
        && !visible_files.is_empty()
    {
        if logs.host_pane_maximized {
            vec![(lead, content_area)]
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![
                    Constraint::Ratio(1, logs.host_views.len() as u32);
                    logs.host_views.len()
                ])
                .split(content_area)
                .iter()
                .copied()
                .enumerate()
                .collect()
        }
    } else {
        Vec::new()
    };
    let host_areas: Vec<_> = host_areas
        .into_iter()
        .enumerate()
        .map(|(position, (index, area))| {
            let chart_height = if position == 0 { 3 } else { 1 };
            if individual_charts && area.width >= 4 && area.height >= chart_height + 3 {
                let parts =
                    Layout::vertical([Constraint::Length(chart_height), Constraint::Min(3)])
                        .split(area);
                render_activity_graph(f, logs, parts[0], Some(index), position == 0);
                (index, parts[1])
            } else {
                (index, area)
            }
        })
        .collect();
    let notice = truncation_notice(logs);
    let reserved_notice_height = usize::from(notice.is_some()) + usize::from(logs.combined);
    let height = (host_areas
        .iter()
        .find_map(|(index, area)| (*index == lead).then_some(*area))
        .unwrap_or(content_area)
        .height
        .saturating_sub(2) as usize)
        .saturating_sub(reserved_notice_height);
    logs.view_height = height;
    logs.scroll = logs.scroll.min(logs.matches.len().saturating_sub(height));
    let no_hosts = !hosts.is_empty() && hosts.iter().all(|host| !logs.host_enabled(host));
    let mut content: Vec<Line> = if logs.empty_grid_cell() {
        vec![Line::from("No log in this cell. Use Shift+arrows to move.")]
    } else if logs.preview_pending() {
        vec![Line::from(
            "Preview updates when browsing pauses; Enter opens now.",
        )]
    } else if no_hosts {
        vec![Line::from(
            "No hosts selected. Use Tab to focus Hosts and include one.",
        )]
    } else if logs.files_focus && !logs.files.is_empty() && visible_files.is_empty() {
        vec![Line::from(
            "No matching files. Clear the name filter with c or port filters with P.",
        )]
    } else if filter_active && !logs.rows.is_empty() && logs.matches.is_empty() {
        vec![Line::from(format!("Filtered by: '{}'", logs.search))]
    } else if filter_active && logs.lines.is_empty() && logs.indexed_filter_completed() {
        vec![Line::from(
            "No cached records match the search or filters in this time window.",
        )]
    } else if logs.lines.is_empty() && (logs.window.start.is_some() || logs.window.end.is_some()) {
        vec![Line::from(
            "No cached log records in the truncation window. Ctrl+T adjusts or clears it.",
        )]
    } else if logs.lines.is_empty() {
        vec![
            Line::from(
                "No cached content for this log. Select a tracking mode with o, then press d to refresh its hosts.",
            ),
            Line::from(
                "m merges this port's error and access logs; \\ hides files; v changes display.",
            ),
            Line::from(
                "t tracks a file; o changes its mode; s checks on-demand files; d refreshes one file.",
            ),
        ]
    } else {
        logs.matches
            .iter()
            .enumerate()
            .skip(logs.scroll)
            .take(height)
            .map(|(position, &index)| {
                let line = if logs.rows.get(index).is_some() || !logs.host_views.is_empty() {
                    logs.display_row(index)
                } else {
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
                    Line::from(Span::styled(text.clone(), Style::default().fg(color)))
                };
                let line = file_search
                    .as_ref()
                    .map_or(line.clone(), |patterns| patterns.highlight(line));
                let line = if logs.show_line_numbers {
                    with_line_number(line, index + 1)
                } else {
                    line
                };
                if !logs.files_focus
                    && host_areas.is_empty()
                    && Some(position) == logs.anchor_position()
                {
                    line.style(Style::default().bg(Color::DarkGray))
                } else {
                    line
                }
            })
            .collect()
    };
    if logs.combined {
        let header = if logs.options.raw {
            "HOST SOURCE / RAW ENTRY".to_owned()
        } else {
            let width = if logs.options.relative { 6 } else { 23 };
            format!("{:<4}{:>5} {:<width$} HOST", "TYPE", "PORT", "TIMESTAMP")
        };
        content.insert(
            0,
            Line::styled(
                header,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        );
    }
    if let Some(notice) = notice.clone() {
        content.insert(0, notice);
    }
    let title = if logs.files_focus && visible_files.is_empty() {
        "No matching files".into()
    } else {
        logs.selected_file()
            .map(|file| {
                format!(
                    "{} / {}{}",
                    file.host,
                    file.name,
                    if logs.selected_cache_is_partial() {
                        " · partial cache"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_else(|| "Cached content".into())
    };
    let title = if logs.has_log_selection() && logs.host_filter_active() {
        format!(
            "SELECTED {} logs · {}/{} hosts · {} · Esc: clear",
            logs.log_selection_count(),
            hosts.iter().filter(|host| logs.host_enabled(host)).count(),
            hosts.len(),
            logs.log_selection_scope_label(),
        )
    } else if logs.has_log_selection() {
        format!(
            "SELECTED {} logs · all hosts · {} · {} · Esc: clear",
            logs.log_selection_count(),
            if logs.options.raw { "raw" } else { "formatted" },
            logs.log_selection_scope_label(),
        )
    } else if logs.combined {
        format!(
            "MERGED port {} · {} · error + access · {} · m: unmerge",
            logs.selected_port().unwrap_or("unselected"),
            logs.discovery_day_label(),
            if logs.options.raw { "raw" } else { "formatted" }
        )
    } else if logs.all_cached {
        "All cached logs (grouped by host/file)".into()
    } else {
        format!("{title} · {}", logs.discovery_day_label())
    };
    let content = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(if logs.combined {
                    Color::Magenta
                } else {
                    Color::White
                }))
                .border_style(if logs.combined {
                    Style::default().fg(Color::Magenta)
                } else {
                    active_border_style(
                        !logs.files_focus,
                        terminal_focused,
                        if filter_active {
                            Color::Red
                        } else {
                            Color::Yellow
                        },
                    )
                }),
        )
        .scroll((
            0,
            if logs.combined && logs.options.wrap {
                0
            } else {
                logs.horizontal
            },
        ));
    if host_areas.is_empty() {
        f.render_widget(content, content_area);
        logs.mouse_regions
            .push((content_area, LogMouseTarget::Content));
    } else {
        for (index, area) in &host_areas {
            let Some(view) = logs.host_views.get(*index) else {
                continue;
            };
            let area = *area;
            let pane_height =
                usize::from(area.height.saturating_sub(2)).saturating_sub(reserved_notice_height);
            let anchor = if *index == lead {
                logs.anchor_position()
            } else {
                logs.host_anchor(view)
            };
            let start = if *index == lead {
                logs.scroll
                    .min(view.matches.len().saturating_sub(pane_height))
            } else {
                anchor
                    .unwrap_or(0)
                    .saturating_sub(pane_height / 2)
                    .min(view.matches.len().saturating_sub(pane_height))
            };
            let mut lines: Vec<Line> =
                if filter_active && !view.rows.is_empty() && view.matches.is_empty() {
                    vec![Line::from(format!("Filtered by: '{}'", logs.search))]
                } else {
                    view.matches
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(pane_height)
                        .map(|(position, &row)| {
                            let line = logs.display_host_row(view, row);
                            let line = file_search
                                .as_ref()
                                .map_or(line.clone(), |patterns| patterns.highlight(line));
                            let line = if logs.show_line_numbers {
                                with_line_number(line, row + 1)
                            } else {
                                line
                            };
                            if Some(position) == anchor {
                                line.style(Style::default().bg(Color::DarkGray))
                            } else {
                                line
                            }
                        })
                        .collect()
                };
            if let Some(notice) = notice.clone() {
                lines.insert(0, notice);
            }
            let title = format!(
                "{}{}{}",
                view.host,
                if *index == lead && logs.host_pane_maximized {
                    " · maximized (z to restore)"
                } else if *index == lead {
                    " · leading (Tab)"
                } else {
                    ""
                },
                if view.keys.iter().any(|key| logs.cache_is_partial(key)) {
                    " · partial cache"
                } else {
                    ""
                }
            );
            let panel = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .title_style(Style::default().fg(Color::White))
                        .border_style(active_border_style(
                            !logs.files_focus && *index == lead,
                            terminal_focused,
                            if filter_active {
                                Color::Red
                            } else {
                                Color::Yellow
                            },
                        )),
                )
                .scroll((0, logs.horizontal));
            f.render_widget(panel, area);
            logs.mouse_regions
                .push((area, LogMouseTarget::Host(*index)));
        }
    }
    let visible_rows = logs.matches.len();
    let total_rows = logs.lines.len();
    let filtered_rows = total_rows.saturating_sub(visible_rows);
    let mut search_help = if logs.files_focus {
        vec![format!(
            "Ports: {}/{} shown · Rows: {visible_rows} visible, {filtered_rows} filtered out · filter: {} · ports: {}",
            port_rows.len(),
            logs.available_port_count(),
            if logs.file_filter.is_empty() {
                "all"
            } else {
                &logs.file_filter
            },
            logs.port_filter_summary(),
        )]
    } else {
        vec![format!(
            "{}{}{} · {visible_rows} visible · {filtered_rows} filtered out of {total_rows} rows · context -{} +{}{}",
            if filter_active {
                "Filtered by: '"
            } else {
                "Search: "
            },
            logs.search,
            if filter_active { "'" } else { "" },
            logs.context_before,
            logs.context_after,
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
    } else if logs.hosts_focus {
        search_help
            .push("Hosts: ↑/↓ select · Space toggle · i only · a all · Tab next pane".into());
    } else if logs.files_focus {
        search_help.push("V: select row · C: select column · Enter: open".into());
        if logs.has_log_selection() {
            search_help.push(format!(
                "{} logs selected · Esc: clear",
                logs.log_selection_count()
            ));
        }
        search_help.push("←/→ or h/l: tracked columns".into());
        search_help.push("Shift+arrows or H/J/K/L: all cells".into());
        search_help.push("Ctrl+←/→ or Ctrl+h/l: days".into());
        if logs.show_untracked {
            search_help.push("0-9: days ago".into());
        }
        search_help.extend([
            "F or /: filter".into(),
            "p: ports".into(),
            "w: hosts".into(),
            "P: clear ports".into(),
            "d: download".into(),
            "o: mode".into(),
            "t: track/untrack".into(),
            "u: untrack".into(),
            "c: clear filter".into(),
        ]);
    } else {
        search_help.extend([
            "/: search".into(),
            "fi/fo: filter · fc: clear · fs/fr/fa: save/recall/add".into(),
            "[/]: context".into(),
        ]);
        if !host_areas.is_empty() {
            search_help.extend([
                "Tab/Shift-Tab: next/previous pane".into(),
                "z: maximize/restore".into(),
            ]);
        }
        if logs.combined {
            search_help.push("n/N: matches".into());
        }
    }
    if let Some((prompt, _)) = logs.filter_dialog().and_then(|dialog| dialog.edit_prompt()) {
        search_help = vec![
            prompt,
            "Enter: apply · Esc: cancel · Tab: layer · -F, term1,term2".into(),
        ];
    } else if !logs.files_focus && logs.content_filter_active() {
        search_help.push(logs.filter_description());
    }
    if logs.time_editing {
        search_help = vec![
            format!("Seek: {}", logs.time_input),
            "Enter: go".into(),
            "Esc: cancel".into(),
        ];
    }
    if logs.filter_dialog().is_none()
        && !logs.file_filter_editing
        && !logs.search_editing
        && !logs.time_editing
    {
        if let Some(time) = logs.anchor_timestamp() {
            search_help.insert(0, format!("Time: {}", time.format("%Y-%m-%d %H:%M:%S%.3f")));
        }
    }
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&search_help))
            .style(Style::default().bg(menu_bar_background_color(terminal_focused))),
        chunks[2],
    );
    if let (Some(area), Some((percent, label))) = (progress_area, progress) {
        f.render_widget(
            Gauge::default()
                .percent(percent)
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Rgb(30, 39, 52)))
                .label(label),
            area,
        );
    }
    let message = if let Some(progress) = logs.activity_status() {
        match logs.parsing_status() {
            Some(parsing) if !logs.message.is_empty() => {
                format!("{progress} · {} · {parsing}", logs.message)
            }
            Some(parsing) => format!("{parsing} | {progress}"),
            None if !logs.message.is_empty() => format!("{progress} · {}", logs.message),
            None => progress,
        }
    } else if let Some(parsing) = logs.parsing_status() {
        parsing
    } else if logs.errors.is_empty() {
        logs.message.clone()
    } else {
        format!(
            "{} | {} host problem(s): {}",
            logs.message,
            logs.errors.len(),
            logs.errors
                .iter()
                .map(|(host, error)| format!("{host}: {error}"))
                .collect::<Vec<_>>()
                .join(" | ")
        )
    };
    let message = if let Some(archive) = logs.archive_status().filter(|_| !logs.busy) {
        format!("{archive} · {message}")
    } else {
        message
    };
    let message = if logs.busy || logs.parsing_status().is_some() {
        message
    } else if let Some(page) = logs.indexed_page_status() {
        format!("{page} · {message}")
    } else if logs.large_view_limited() {
        format!("Large view limited to the latest records; narrow the time window · {message}")
    } else if logs.window.start.is_some() || logs.window.end.is_some() {
        format!("Window: {} · {}", logs.window_label(), message)
    } else {
        message
    };
    let message = logs.service_status().unwrap_or(message);
    f.render_widget(
        Paragraph::new(message).style(Style::default().fg(Color::Cyan)),
        chunks[3],
    );
    if let Some((_, cursor)) = logs.filter_dialog().and_then(|dialog| dialog.edit_prompt()) {
        f.set_cursor_position((
            chunks[2].x + cursor.min(usize::from(chunks[2].width.saturating_sub(1))) as u16,
            chunks[2].y,
        ));
    } else if logs.file_filter_editing || logs.search_editing || logs.time_editing {
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
            .min(usize::from(chunks[2].width.saturating_sub(1))) as u16;
        f.set_cursor_position((chunks[2].x + column, chunks[2].y));
    }
    if let Some((names, selected, add)) = logs.filter_dialog().and_then(|dialog| dialog.choices()) {
        let screen = f.area();
        let width = screen.width.saturating_sub(4).min(60).max(1);
        let height = (names.len() as u16 + 2)
            .min(screen.height.saturating_sub(2))
            .max(3);
        let area = Rect::new(
            screen.x + screen.width.saturating_sub(width) / 2,
            screen.y + screen.height.saturating_sub(height) / 2,
            width,
            height,
        );
        f.render_widget(Clear, area);
        let items: Vec<_> = names
            .iter()
            .map(|name| ListItem::new(name.as_str()))
            .collect();
        let title = if add {
            " Add saved filter · Enter / Esc "
        } else {
            " Recall filter · Enter / Esc "
        };
        let mut state = ListState::default().with_selected(Some(selected));
        f.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                .highlight_symbol("› "),
            area,
            &mut state,
        );
    }
    if logs.options_open {
        render_options(f, logs, terminal_focused);
    }
    if let Some(dialog) = &logs.truncation_dialog {
        render_truncation_dialog(f, dialog, terminal_focused);
    }
    if let Some(detail) = &logs.entry_detail {
        render_entry_detail(f, detail);
    }
    if logs.download_queue_open {
        render_download_queue(f, logs);
    }
    if logs.errors_open {
        render_errors(f, logs);
    }
}

fn render_errors(f: &mut Frame, logs: &LogViewer) {
    let screen = f.area();
    let width = screen.width.saturating_sub(2).min(120).max(1);
    let height = screen.height.saturating_sub(2).min(28).max(1);
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let mut lines = Vec::new();
    if let Some(status) = logs.service_status() {
        lines.push(Line::styled(status, Style::default().fg(Color::Yellow)));
        if let Some(hint) = logs.service_hint() {
            lines.push(Line::from(hint));
        }
        lines.push(Line::default());
    }
    if logs.errors.is_empty() {
        lines.push(Line::from("No current log errors."));
    }
    for (host, error) in &logs.errors {
        lines.push(Line::styled(
            host.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        lines.extend(error.lines().map(|line| Line::from(line.to_owned())));
        lines.push(Line::default());
    }
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((logs.error_scroll, 0))
            .block(
                Block::bordered()
                    .title(" Log errors · ↑/↓ scroll · R retry · Esc close ")
                    .border_style(Style::default().fg(Color::Red)),
            ),
        area,
    );
}

fn render_download_queue(f: &mut Frame, logs: &LogViewer) {
    let screen = f.area();
    let width = screen.width.saturating_sub(4).min(120);
    let height = screen.height.saturating_sub(2).min(24);
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Downloads · Enter/r: start now · x: remove queued · Esc: close ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);
    let summary = logs
        .activity_status()
        .unwrap_or_else(|| "No operation running.".into());
    f.render_widget(
        Paragraph::new(summary).style(Style::default().fg(Color::Cyan)),
        sections[0],
    );
    let rows = logs.download_queue_rows();
    let selected = logs
        .download_queue_selected
        .min(rows.len().saturating_sub(1));
    let rows = if rows.is_empty() {
        vec!["No downloads waiting.".into()]
    } else {
        rows
    };
    let items: Vec<_> = rows.into_iter().map(ListItem::new).collect();
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
            .highlight_symbol("› "),
        sections[1],
        &mut state,
    );
    f.render_widget(
        Paragraph::new(logs.message.as_str()).wrap(Wrap { trim: false }),
        sections[2],
    );
}

fn render_activity_graph(
    f: &mut Frame,
    logs: &mut LogViewer,
    area: Rect,
    series: Option<usize>,
    show_labels: bool,
) {
    let width = usize::from(area.width.saturating_sub(2));
    logs.activity_graph.poll(width);
    let title =
        if logs.activity_graph.pending() && logs.activity_graph.selection_columns().is_none() {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let tick = chrono::Utc::now().timestamp_millis().div_euclid(100);
            format!(" Log Activity {} ", frames[tick.rem_euclid(10) as usize])
        } else if let Some(chart) = logs.activity_graph.displayed_chart() {
            format!(" Log Activity · {} ", chart.interval)
        } else {
            " Log Activity ".to_owned()
        };
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(u16::from(show_labels)),
        area.width.saturating_sub(2),
        area.height.saturating_sub(u16::from(show_labels)),
    );
    if show_labels {
        f.render_widget(
            Paragraph::new(title).style(Style::default().fg(Color::Cyan)),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    if logs.activity_graph.displayed_chart().is_some() {
        logs.mouse_regions.push((inner, LogMouseTarget::Activity));
    }
    let selection = logs.activity_graph.selection_columns();
    if let Some(chart) = logs.activity_graph.displayed_chart() {
        let format = if width >= 39 {
            "%Y-%m-%d %H:%M:%S"
        } else if width >= 17 {
            "%H:%M:%S"
        } else {
            ""
        };
        let start = chart.start.format(format).to_string();
        let end = chart.end.format(format).to_string();
        let labels = format!(
            "{start}{}{end}",
            " ".repeat(width - start.len() - end.len())
        );
        let backgrounds = [Color::Rgb(78, 64, 16), Color::Rgb(109, 87, 20)];
        let bar_line = |bar: &str| {
            let mut spans = Vec::new();
            let mut characters = String::new();
            let mut previous = None;
            for (column, (character, &band)) in bar.chars().zip(&chart.bands).enumerate() {
                let selected =
                    selection.is_some_and(|(start, end)| (start..=end).contains(&column));
                let background = if selected {
                    Color::Rgb(75, 110, 165)
                } else {
                    backgrounds[usize::from(band)]
                };
                if previous.is_some_and(|old| old != background) {
                    spans.push(Span::styled(
                        std::mem::take(&mut characters),
                        Style::default().fg(Color::White).bg(previous.unwrap()),
                    ));
                }
                characters.push(character);
                previous = Some(background);
            }
            if let Some(background) = previous {
                spans.push(Span::styled(
                    characters,
                    Style::default().fg(Color::White).bg(background),
                ));
            }
            Line::from(spans)
        };
        let mut lines = Vec::new();
        if show_labels {
            lines.push(bar_line(&labels));
        }
        if let Some(index) = series {
            if let Some(bar) = chart.series_bars.get(index) {
                lines.push(bar_line(bar));
            }
        } else {
            lines.push(bar_line(&chart.bars[0]));
            lines.push(bar_line(&chart.bars[1]));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn render_entry_detail(f: &mut Frame, detail: &crate::logs::LogEntryDetail) {
    let screen = f.area();
    let width = screen.width.saturating_sub(4).min(100).max(1);
    let height = screen.height.saturating_sub(4).min(26).max(1);
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, area);
    let title = format!(
        " Log entry · w wrap {} · r raw {} · e edit ",
        if detail.wrap { "on" } else { "off" },
        if detail.raw { "on" } else { "off" },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    f.render_widget(
        Paragraph::new(detail.header.clone()).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );
    let body = if detail.raw {
        &detail.raw_text
    } else {
        &detail.formatted_text
    };
    let paragraph = Paragraph::new(body.as_str()).scroll((detail.scroll, 0));
    f.render_widget(
        if detail.wrap {
            paragraph.wrap(Wrap { trim: false })
        } else {
            paragraph
        },
        chunks[1],
    );
    f.render_widget(
        Paragraph::new("↑/↓ scroll  u/d page  v/Esc close")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn render_truncation_dialog(
    f: &mut Frame,
    dialog: &crate::logs::TruncationDialog,
    terminal_focused: bool,
) {
    let screen = f.area();
    let width = screen.width.saturating_sub(2).min(76);
    let height = screen.height.saturating_sub(2).min(12);
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (index, label) in [
        "Start (inclusive)",
        "End (inclusive)",
        "Adjustment in minutes",
    ]
    .iter()
    .enumerate()
    {
        lines.push(
            Line::from(format!(
                "{} {}: {}",
                if dialog.selected == index { "›" } else { " " },
                label,
                dialog.fields[index]
            ))
            .style(Style::default().fg(if dialog.selected == index {
                Color::Yellow
            } else {
                Color::White
            })),
        );
        lines.push(Line::from(""));
    }
    lines.push(Line::from("YYYY-MM-DD HH:MM:SS[.mmm]; blank means open"));
    lines.push(Line::from(
        "Tab: field · ↑/↓: +/- minutes · Ctrl+U: clear field",
    ));
    lines.push(Line::from(
        "Ctrl+R: clear both · Enter: apply · Esc: close (keeps draft)",
    ));
    if !dialog.error.is_empty() {
        lines.push(Line::from(dialog.error.clone()).style(Style::default().fg(Color::Red)));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Truncation · server time")
                .border_style(active_border_style(true, terminal_focused, Color::Cyan)),
        ),
        area,
    );
}

fn render_options(f: &mut Frame, logs: &LogViewer, terminal_focused: bool) {
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
                "Ports · all → include only + → exclude -"
            } else {
                "Combined log display"
            })
            .border_style(active_border_style(true, terminal_focused, Color::Cyan)),
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
        Paragraph::new(styled_menu_bar_items(&if logs.options_ports {
            vec![
                "j/k: select".into(),
                "Enter: all/+/-".into(),
                "P: clear".into(),
                "i: toggle + only".into(),
                "Esc: close".into(),
            ]
        } else {
            vec![
                "j/k: select".into(),
                "Enter/Space: change".into(),
                "Esc: close".into(),
            ]
        }))
        .style(Style::default().bg(menu_bar_background_color(terminal_focused))),
        rows[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::LogFile;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    #[ignore = "manual release-build rendering benchmark"]
    fn benchmark_large_log_inventory_render() {
        let mut logs = LogViewer::default();
        for host in 0..20 {
            for port in 8000..8020 {
                for day in 0..7 {
                    for kind in ["ErrorLog", "AccessLog", "RequestLog"] {
                        let suffix = if day == 0 {
                            String::new()
                        } else {
                            format!("_{day}")
                        };
                        let name = format!("{port}_{kind}{suffix}.txt");
                        logs.files.push(LogFile {
                            host: format!("host-{host:02}"),
                            path: format!("/Logs/{name}"),
                            name,
                            size: 1024,
                            modified: "2026-10-31T12:00:00Z".into(),
                            present: true,
                        });
                    }
                }
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        let cold = std::time::Instant::now();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let cold = cold.elapsed();
        let warm = std::time::Instant::now();
        for _ in 0..10 {
            terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        }
        eprintln!(
            "{} files: cold {cold:?}, warm average {:?}",
            logs.files.len(),
            warm.elapsed() / 10
        );
    }

    #[test]
    fn log_groups_highlight_only_selected_cells_and_show_the_search_scope() {
        let mut logs = LogViewer::default();
        logs.files = [
            "8000_ErrorLog.txt",
            "8000_AccessLog.txt",
            "8000_ErrorLog_1.txt",
            "9000_ErrorLog.txt",
        ]
        .into_iter()
        .map(|name| LogFile {
            host: "host".into(),
            path: format!("/Logs/{name}"),
            name: name.into(),
            size: 10,
            modified: String::new(),
            present: true,
        })
        .collect();
        let mut terminal = Terminal::new(TestBackend::new(120, 26)).unwrap();
        logs.select_log_group(true);
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let files_area = logs
            .mouse_regions
            .iter()
            .find_map(|(area, target)| {
                matches!(target, LogMouseTarget::Files { .. }).then_some(*area)
            })
            .unwrap();
        let first_row = files_area.y + 3;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(9, first_row)].bg, Color::Magenta);
        assert_eq!(buffer[(17, first_row)].bg, Color::Magenta);
        assert_ne!(buffer[(11, first_row)].bg, Color::Magenta);
        assert_ne!(buffer[(9, first_row + 1)].bg, Color::Magenta);
        let screen = (0..26)
            .map(|y| {
                (0..120)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("SELECTED 2 logs · all hosts"));
        assert!(screen.contains("V/C: select · t: track · o: mode"));

        logs.select_log_group(false);
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(9, first_row)].bg, Color::Magenta);
        assert_eq!(buffer[(9, first_row + 1)].bg, Color::Magenta);
        assert_ne!(buffer[(17, first_row)].bg, Color::Magenta);
        assert_ne!(buffer[(11, first_row)].bg, Color::Magenta);

        logs.clear_log_selection();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        assert_ne!(
            terminal.backend().buffer()[(17, first_row)].bg,
            Color::Magenta
        );
    }

    #[test]
    fn grid_shows_seven_days_and_selection_layers() {
        let mut logs = LogViewer::default();
        logs.files = ["ErrorLog.txt", "ErrorLog_1.txt", "8000_AccessLog_1.txt"]
            .into_iter()
            .map(|name| LogFile {
                host: "host".into(),
                path: format!("/Logs/{name}"),
                name: name.into(),
                size: 0,
                modified: String::new(),
                present: true,
            })
            .collect();
        logs.files_focus = true;
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..120)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(2).contains("TODAY") && row(2).contains("DAY 6"));
        assert!(row(4).contains("ROOT"));
        assert_eq!(buffer[(9, 4)].bg, Color::LightBlue);
        assert_eq!(buffer[(9, 4)].fg, Color::Black);
        assert_ne!(buffer[(17, 4)].bg, buffer[(17, 5)].bg);
        logs.files_focus = false;
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        assert_eq!(
            logs.mouse_regions
                .iter()
                .find(|(_, target)| matches!(target, LogMouseTarget::Files { .. }))
                .unwrap()
                .0
                .width,
            17
        );
    }

    #[test]
    fn truncation_notice_shows_an_end_only_bound() {
        let mut logs = LogViewer::default();
        logs.window.end = chrono::NaiveDate::from_ymd_opt(2026, 10, 14)
            .unwrap()
            .and_hms_opt(8, 9, 0);
        let text = truncation_notice(&logs).unwrap().to_string();
        assert!(text.contains("open to 2026-10-14 08:09:00.000"));
    }

    #[test]
    fn files_frame_keeps_column_header_visible_when_rows_scroll() {
        let mut logs = LogViewer::default();
        logs.show_untracked = true;
        logs.files = (8000..8025)
            .map(|port| {
                let name = format!("{port}_ErrorLog.txt");
                LogFile {
                    host: "one".into(),
                    path: format!("/Logs/{name}"),
                    name,
                    size: 10,
                    modified: String::new(),
                    present: true,
                }
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
        for last in [false, true] {
            if last {
                logs.select_visible_last();
            }
            terminal
                .draw(|frame| render(frame, &mut logs, true))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| (0..50).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            assert!(row(1).contains("Files"));
            assert!(
                row(2).contains("PORT")
                    && row(2).contains("Err")
                    && row(2).contains("Acc")
                    && row(2).contains("Req")
            );
            if last {
                assert!((3..18).any(|y| row(y).contains("8024")));
                assert!(!(3..18).any(|y| row(y).contains("8000")));
            }
        }
    }

    #[test]
    fn empty_tracked_logs_are_gray_and_modes_are_explained() {
        let mut logs = LogViewer::default();
        logs.files = [("ErrorLog.txt", 0), ("8000_ErrorLog.txt", 10)]
            .into_iter()
            .map(|(name, size)| LogFile {
                host: "one".into(),
                path: format!("/Logs/{name}"),
                name: name.into(),
                size,
                modified: "2026-10-14T12:00:00Z".into(),
                present: true,
            })
            .collect();
        logs.errors.insert("one".into(), "permission denied".into());
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let empty = rows
            .iter()
            .position(|row| row.contains("----") && row.contains("⬇"))
            .unwrap() as u16;
        let nonempty = rows
            .iter()
            .position(|row| row.contains("8000") && row.contains("⬇"))
            .unwrap() as u16;
        let marker = rows[empty as usize].find("[⬇]").unwrap() as u16;
        assert_eq!(buffer[(marker, empty)].fg, Color::DarkGray);
        assert_eq!(buffer[(marker, nonempty)].fg, Color::DarkGray);
        assert!(rows.iter().any(|row| row.contains("0 B  n/a")));
        assert!(rows.iter().any(|row| row.contains("Files · CURRENT DAY")));
        let title = rows
            .iter()
            .position(|row| row.contains("Files · CURRENT DAY"))
            .unwrap() as u16;
        assert_eq!(buffer[(0, title)].fg, Color::Yellow);
        assert!(
            rows.iter()
                .any(|row| row.contains("↓ on-demand  ↻ live  ◐ partial"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("✓ green <5m  ✓ yellow <30m"))
        );
        assert!(!rows.iter().any(|row| row.contains("HTTP bodies")));
        assert!(!rows.iter().any(|row| row.contains("Cache:")));
        assert!(rows.iter().any(|row| row.contains("permission denied")));
    }

    #[test]
    fn selected_port_column_keeps_tracked_colors_and_blacks_untracked_cells() {
        let mut logs = LogViewer::default();
        logs.files = ["ErrorLog.txt", "8000_ErrorLog.txt", "9000_ErrorLog.txt"]
            .into_iter()
            .map(|name| LogFile {
                host: "one".into(),
                path: format!("/Logs/{name}"),
                name: name.into(),
                size: 10,
                modified: "2026-10-14T12:00:00Z".into(),
                present: true,
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();

        for (port, marker, expected_fg) in [
            ("8000", "[⬇]", Color::DarkGray),
            ("9000", "[·]", Color::Rgb(0, 0, 0)),
        ] {
            let y = rows
                .iter()
                .position(|row| row.contains(port) && row.contains(marker))
                .unwrap() as u16;
            let x = rows[y as usize].find(marker).unwrap() as u16;
            assert_eq!(buffer[(x, y)].fg, expected_fg);
            assert_eq!(buffer[(x, y)].bg, Color::DarkGray);
        }
    }

    #[test]
    fn discovery_screen_shows_the_day_and_navigation_hint() {
        let mut logs = LogViewer::default();
        logs.show_untracked = true;
        logs.discovery_days_ago = 1;
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("Files · YESTERDAY"));
        assert!(screen.contains("Ctrl←/→: days"));
    }

    #[test]
    fn selected_host_details_can_scroll_without_hiding_the_file_list() {
        let mut logs = LogViewer::default();
        logs.files = (1..=20)
            .map(|i| LogFile {
                host: format!("host-{i:02}"),
                path: "/Logs/ErrorLog.txt".into(),
                name: "ErrorLog.txt".into(),
                size: i,
                modified: String::new(),
                present: true,
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let first: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(first.contains("1 B  n/a"));
        assert!(first.contains("h1=host-01"));
        assert!(!first.contains("h20=host-20"));
        assert!(first.contains(",/."));
        assert!(first.contains("Files · CURRENT DAY"));
        for _ in 0..20 {
            logs.scroll_host_details(true);
        }
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let last: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(last.contains("20 B  n/a"));
        assert!(last.contains("h20=host-20"));
        assert!(last.contains("Files · CURRENT DAY"));
    }

    #[test]
    fn long_hostnames_are_readable_in_the_pane_above_files() {
        let mut logs = LogViewer::default();
        logs.files = (1..=6)
            .map(|i| LogFile {
                host: format!("taxtech-nonprod-ml{i}.nl.ema.pwcinternal.com"),
                path: "/Logs/8000_ErrorLog.txt".into(),
                name: "8000_ErrorLog.txt".into(),
                size: i * 1024,
                modified: String::new(),
                present: true,
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..72).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            rows.iter()
                .any(|row| row.contains("h1=taxtech-nonprod-ml1.nl.ema.pwcinternal.com"))
        );
        let host_row = rows
            .iter()
            .position(|row| row.contains("h1=taxtech-nonprod-ml1"))
            .unwrap();
        let file_row = rows
            .iter()
            .position(|row| row.contains("Files · CURRENT DAY"))
            .unwrap();
        assert!(host_row < file_row);
        assert!(rows[host_row].contains("1.0 KiB"));
        assert!(rows[host_row].contains("n/a"));
        logs.scroll_host_details(true);
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("h6=taxtech-nonprod-ml6.nl.ema.pwcinternal.com"));
    }

    #[test]
    fn combined_logs_fill_the_width_when_sidebar_is_hidden() {
        let mut logs = LogViewer::default();
        logs.combined = true;
        logs.sidebar_visible = false;
        logs.files_focus = false;
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
            truncation_marker: false,
        }];
        logs.matches = vec![0];
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
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
        let row = (0..20)
            .find(|&y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
                    .contains("ERR  9060")
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(1, row)].fg, Color::LightRed);
        for (width, height) in [(20, 5), (1, 1)] {
            logs.options_open = true;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        }
    }

    #[test]
    fn log_view_handles_small_terminals_and_scrolls_beyond_u16_rows() {
        let mut logs = LogViewer::default();
        logs.lines = (0..70_000).map(|i| format!("line-{i}")).collect();
        logs.matches = (0..70_000).collect();
        logs.scroll = 69_980;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("line-69980"));
        assert!(!screen.contains("HTTP bodies"));
        assert!(!screen.contains("Server read estimate"));
        assert!(!screen.contains("Cache:"));
        assert_eq!(logs.view_height, 21);
        assert!(logs.scroll > u16::MAX as usize);
        for (width, height) in [(60, 12), (20, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        }
    }

    #[test]
    fn activity_chart_has_two_bar_rows_and_stays_fixed_while_logs_scroll() {
        let mut logs = LogViewer::default();
        logs.sidebar_visible = false;
        logs.files_focus = false;
        let start = chrono::NaiveDate::from_ymd_opt(2026, 10, 23)
            .unwrap()
            .and_hms_opt(8, 0, 0)
            .unwrap();
        let end = start + chrono::Duration::days(7);
        logs.lines = vec!["entry".into(); 5];
        logs.rows = [start, start, start, start, end]
            .into_iter()
            .map(|timestamp| crate::logs::ViewRow {
                line: Line::from("entry"),
                timestamp: Some(timestamp),
                error: false,
                start: true,
                search_match: false,
                truncation_marker: false,
            })
            .collect();
        logs.filter();
        logs.activity_graph.poll(78);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while logs.activity_graph.pending() {
            assert!(
                std::time::Instant::now() < deadline,
                "activity worker stalled"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
            logs.activity_graph.poll(78);
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut logs, true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(1).contains("Log Activity"));
        assert!(row(1).contains("1 day"));
        assert_eq!(buffer[(0, 1)].symbol(), " ");
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_eq!(buffer[(0, 3)].symbol(), " ");
        assert_eq!(buffer[(0, 4)].symbol(), " ");
        assert!(row(2).contains("2026-10-23 08:00:00"));
        assert!(row(2).contains("2026-10-30 08:00:00"));
        assert_eq!(buffer[(1, 3)].symbol(), "█");
        assert_eq!(buffer[(1, 4)].symbol(), "█");
        assert_eq!(buffer[(40, 3)].symbol(), " ");
        assert_eq!(buffer[(40, 4)].symbol(), " ");
        assert_eq!(buffer[(78, 3)].symbol(), " ");
        assert_eq!(buffer[(78, 4)].symbol(), "▄");
        assert_eq!(buffer[(1, 3)].fg, Color::White);
        assert_eq!(buffer[(1, 2)].fg, Color::White);
        let first_bg = buffer[(1, 3)].bg;
        let other_bg = buffer[(15, 3)].bg;
        assert_ne!(first_bg, other_bg);
        assert_eq!(first_bg, buffer[(1, 2)].bg);
        assert_eq!(other_bg, buffer[(15, 2)].bg);
        assert_eq!(first_bg, buffer[(1, 4)].bg);
        assert_eq!(other_bg, buffer[(15, 4)].bg);
        assert_eq!(buffer[(15, 3)].symbol(), " ");
        assert!(row(7).contains("entry"));
        let bars = (row(3), row(4));
        let cached = logs.activity_graph.chart.as_ref().unwrap().bars[0].as_ptr();
        logs.scroll = 2;
        logs.horizontal = 2;
        terminal
            .draw(|frame| render(frame, &mut logs, true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert_eq!((row(3), row(4)), bars);
        assert_eq!(
            logs.activity_graph.chart.as_ref().unwrap().bars[0].as_ptr(),
            cached
        );
    }
}
