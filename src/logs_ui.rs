use crate::logs::{LogMouseTarget, LogViewer, TrackingMode, format_bytes, presentation::Patterns};
use crate::ui::{active_border_style, menu_bar_background_color, styled_menu_bar_items};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
    },
};

const SIDEBAR_WIDTH: u16 = 50;

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
    let filter_active = !logs.search.is_empty()
        && (!logs.combined
            || logs.options.search_mode == crate::logs::presentation::SearchMode::Filter);
    let file_search = (!logs.combined && !logs.search.is_empty())
        .then(|| Patterns::new(&format!("/{}/i", regex::escape(&logs.search))));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    let mut heading = vec![
        format!(
            "Logs: {}/{}/collection {}{}",
            if logs.combined { "merged" } else { "file" },
            if logs.following {
                "following"
            } else {
                "reviewing"
            },
            logs.scope_label(),
            if logs.busy { " (sync)" } else { "" },
        ),
        "^.: previous".into(),
        if logs.combined {
            "m: unmerge".into()
        } else {
            "m: merge".into()
        },
        "\\: files".into(),
        if logs.combined {
            "v: display".into()
        } else if logs.files_focus {
            "Enter: open".into()
        } else {
            "d/u: page".into()
        },
        "C: clear cache".into(),
        "?: help".into(),
        "q: close".into(),
    ];
    if logs.files_focus {
        heading.extend([
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
    if logs.combined {
        heading.push("D: download both".into());
    }
    if !logs.environment_indicator.is_empty() {
        heading.insert(0, logs.environment_indicator.clone());
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
            let width = if chunks[1].width > 60 {
                SIDEBAR_WIDTH.min(chunks[1].width.saturating_sub(24))
            } else {
                chunks[1].width / 2
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
            logs.discovery_day_label()
        )),
        Line::from("←/→: tracked · Shift←/→: all · Ctrl←/→: days"),
        Line::from(if logs.combined {
            "D: download Err+Acc  o: mode  u: untrack"
        } else {
            "d: download  o: mode  u: untrack  r: day keys"
        }),
        Line::from(format!(
            "Ports: {} (p: choose, P: clear)",
            logs.port_filter_summary()
        )),
    ];
    let selected_group = logs.selected_visible_index();
    let host_legend_capacity = if selected_group.is_some() {
        (if logs.combined && logs.selected_display_column() == 0 {
            logs.merged_host_details().len()
        } else {
            logs.selected_host_details().len()
        })
        .min(6)
        .min(usize::from(panes[0].height.saturating_sub(9)))
    } else {
        0
    };
    let sidebar = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length((control_lines.len() + host_legend_capacity) as u16),
        ])
        .split(panes[0]);
    let port_rows = logs.visible_port_rows();
    let visible_files: Vec<usize> = port_rows
        .iter()
        .flat_map(|(_, cells)| cells.iter().flatten().copied())
        .collect();
    let group_summaries = logs.group_summaries(chrono::Utc::now());
    let merged_modes = logs.combined.then(|| logs.merged_modes_by_port());
    if let (Some(file), Some((port, _))) = (
        logs.selected_file(),
        selected_group.and_then(|row| port_rows.get(row)),
    ) {
        let kind = ["Err", "Acc", "Req"][logs.selected_kind_column()];
        if logs.combined && logs.selected_display_column() == 0 {
            control_lines[2] = Line::from(format!("Selected {port} Mrg · D: download both"));
        } else if let Some((_, size, age)) = group_summaries.get(file.name.as_str()) {
            control_lines[2] = Line::from(format!(
                "Selected {port} {kind}: {} · last update {age}",
                format_bytes(*size)
            ));
        }
    }
    let mut items: Vec<ListItem> = port_rows
        .iter()
        .enumerate()
        .map(|(row, (port, cells))| {
            let row_selected = selected_group == Some(row);
            let mut spans = vec![Span::styled(
                format!("{port:<6} "),
                Style::default()
                    .fg(Color::White)
                    .bg(if row_selected {
                        Color::DarkGray
                    } else {
                        Color::Reset
                    })
                    .add_modifier(Modifier::BOLD),
            )];
            let displayed: Vec<(Option<usize>, Option<usize>)> = if logs.combined {
                vec![(cells[0], cells[1]), (cells[2], None)]
            } else {
                cells.iter().map(|index| (*index, None)).collect()
            };
            for (column, (file_index, _second_index)) in displayed.into_iter().enumerate() {
                let selected = logs.selected_display_column() == column;
                let (label, style, tracked) = if logs.combined && column == 0 {
                    let (error, access) = merged_modes
                        .as_ref()
                        .and_then(|modes| modes.get(port))
                        .copied()
                        .unwrap_or((None, None));
                    let tracked = [error, access]
                        .into_iter()
                        .flatten()
                        .any(|mode| mode != TrackingMode::Untracked);
                    let marker =
                        |mode: Option<TrackingMode>| mode.map_or("-", |mode| mode.marker());
                    (
                        format!("[{}/{}]", marker(error), marker(access)),
                        Style::default().fg(if tracked {
                            Color::Green
                        } else {
                            Color::DarkGray
                        }),
                        tracked,
                    )
                } else {
                    file_index
                        .and_then(|index| logs.files.get(index))
                        .map(|file| {
                            let mode = logs.tracking_mode(file);
                            let style = match mode {
                                TrackingMode::Live => Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                                TrackingMode::OnDemand => Style::default()
                                    .fg(Color::Green)
                                    .add_modifier(Modifier::BOLD),
                                TrackingMode::Partial => Style::default().fg(Color::Yellow),
                                TrackingMode::Untracked => Style::default().fg(Color::DarkGray),
                            };
                            (
                                format!("[{}]", mode.marker()),
                                style,
                                mode != TrackingMode::Untracked,
                            )
                        })
                        .unwrap_or_else(|| {
                            (
                                "   -   ".into(),
                                Style::default().fg(Color::DarkGray),
                                false,
                            )
                        })
                };
                let style = if selected {
                    let style = if tracked {
                        style
                    } else {
                        style.fg(Color::Rgb(0, 0, 0))
                    };
                    style.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                } else if row_selected {
                    style.bg(Color::Rgb(34, 43, 58))
                } else {
                    style
                };
                spans.push(Span::styled(format!("{label:<7}"), style));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let capacity = host_legend_capacity.min(usize::from(sidebar[0].height.saturating_sub(4)));
    let (detail_items, host_legend, count, offset) = if logs.combined
        && logs.selected_display_column() == 0
    {
        let details = if selected_group.is_some() {
            logs.merged_host_details()
        } else {
            Vec::new()
        };
        let count = details.len();
        let offset = logs.host_detail_offset.min(count.saturating_sub(capacity));
        let shown: Vec<_> = details.into_iter().skip(offset).take(capacity).collect();
        let describe = |file: Option<&crate::logs::LogFile>, result: Option<bool>| {
            let size = match file {
                Some(file) if file.present => format_bytes(file.size),
                Some(_) => "offline".into(),
                None => "n/a".into(),
            };
            format!(
                "{size}{}",
                match result {
                    Some(true) => "✓",
                    Some(false) => "!",
                    None => "",
                }
            )
        };
        let items = shown
            .iter()
            .map(|(alias, _, error, access, error_result, access_result)| {
                ListItem::new(
                    Line::from(vec![
                        Span::styled(format!("  {alias:<3} "), Style::default().fg(Color::White)),
                        Span::styled(
                            format!("E:{} ", describe(*error, *error_result)),
                            Style::default().fg(if *error_result == Some(false) {
                                Color::Red
                            } else {
                                Color::Cyan
                            }),
                        ),
                        Span::styled(
                            format!("A:{}", describe(*access, *access_result)),
                            Style::default().fg(if *access_result == Some(false) {
                                Color::Red
                            } else {
                                Color::Cyan
                            }),
                        ),
                    ])
                    .style(Style::default().bg(Color::Rgb(24, 32, 46))),
                )
            })
            .collect();
        let host_legend = shown
            .iter()
            .map(|(alias, host, ..)| {
                Line::from(vec![
                    Span::styled(format!("{alias}="), Style::default().fg(Color::Yellow)),
                    Span::styled((*host).to_owned(), Style::default().fg(Color::White)),
                ])
                .style(Style::default().bg(Color::Rgb(24, 32, 46)))
            })
            .collect();
        (items, host_legend, count, offset)
    } else {
        let details = if selected_group.is_some() {
            logs.selected_host_details()
        } else {
            Vec::new()
        };
        let count = details.len();
        let offset = logs.host_detail_offset.min(count.saturating_sub(capacity));
        let now = chrono::Utc::now();
        let shown: Vec<_> = details.into_iter().skip(offset).take(capacity).collect();
        let items: Vec<ListItem> = shown
            .iter()
            .map(|(alias, _host, file, result)| {
                let marker = match result {
                    Some(true) => "  ok",
                    Some(false) => "  failed",
                    None => "",
                };
                let size = match file {
                    Some(file) if file.present => format_bytes(file.size),
                    Some(_) => "unavailable".into(),
                    None => "not listed".into(),
                };
                let age = file
                    .map(|file| logs.cache_age(file, now))
                    .unwrap_or_else(|| "n/a".into());
                ListItem::new(
                    Line::from(vec![
                        Span::styled(
                            format!("  {alias:<3} {size:>10}  "),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(age, Style::default().fg(Color::Cyan)),
                        Span::styled(
                            marker,
                            Style::default().fg(if *result == Some(false) {
                                Color::Red
                            } else {
                                Color::Green
                            }),
                        ),
                    ])
                    .style(Style::default().bg(Color::Rgb(24, 32, 46))),
                )
            })
            .collect();
        let host_legend: Vec<Line> = shown
            .iter()
            .map(|(alias, host, _, _)| {
                Line::from(vec![
                    Span::styled(format!("{alias}="), Style::default().fg(Color::Yellow)),
                    Span::styled((*host).to_owned(), Style::default().fg(Color::White)),
                ])
                .style(Style::default().bg(Color::Rgb(24, 32, 46)))
            })
            .collect();
        (items, host_legend, count, offset)
    };
    let detail_count = detail_items.len();
    logs.host_detail_count = count;
    logs.host_detail_capacity = capacity;
    logs.host_detail_offset = offset;
    if count > capacity {
        control_lines[3] = Line::from(format!(
            "Ports: {}  ,/.: hosts {}-{}/{}",
            logs.port_filter_summary(),
            offset + 1,
            offset + capacity,
            count
        ));
    }
    if let Some(selected) = selected_group {
        items.splice(selected + 1..selected + 1, detail_items);
    }
    let mut legend_lines = host_legend;
    legend_lines.extend(control_lines);
    let header_style = |column: usize| {
        if logs.selected_display_column() == column {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        }
    };
    let mut files_header_spans = vec![Span::styled(
        "  PORT   ",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )];
    if logs.combined {
        files_header_spans.extend([
            Span::styled("Mrg    ", header_style(0)),
            Span::styled("Req", header_style(1)),
        ]);
    } else {
        files_header_spans.extend([
            Span::styled("Err    ", header_style(0)),
            Span::styled("Acc    ", header_style(1)),
            Span::styled("Req", header_style(2)),
        ]);
    }
    let files_header = Line::from(files_header_spans);
    let files_block = Block::default()
        .borders(Borders::ALL)
        .title(" Files ")
        .border_style(active_border_style(
            logs.files_focus,
            terminal_focused,
            Color::Yellow,
        ));
    let inner = files_block.inner(sidebar[0]);
    let header_height = inner.height.min(1);
    let header_area = Rect::new(inner.x, inner.y, inner.width, header_height);
    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(header_height),
        inner.width,
        inner.height.saturating_sub(header_height),
    );
    let files = List::new(items)
        .highlight_style(Style::default())
        .highlight_symbol("› ")
        .highlight_spacing(HighlightSpacing::Always);
    let list_height = usize::from(list_area.height);
    let offset = selected_group.map_or(0, |selected| {
        (selected + detail_count + 1).saturating_sub(list_height)
    });
    let mut selection = ListState::default().with_offset(offset);
    selection.select(selected_group);
    if logs.sidebar_visible {
        f.render_widget(files_block, sidebar[0]);
        f.render_widget(Paragraph::new(files_header), header_area);
        f.render_stateful_widget(files, list_area, &mut selection);
        logs.mouse_regions.push((
            sidebar[0],
            LogMouseTarget::Files {
                offset: selection.offset(),
                selected: selected_group,
                details: detail_count,
            },
        ));
        f.render_widget(
            Paragraph::new(legend_lines).style(Style::default().fg(Color::White)),
            sidebar[1],
        );
    }
    let lead = logs.leading_host_index().unwrap_or(0);
    let host_areas = if !logs.combined
        && !logs.all_cached
        && logs.host_views.len() > 1
        && !visible_files.is_empty()
    {
        if logs.host_pane_maximized {
            vec![(lead, panes[1])]
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![
                    Constraint::Ratio(1, logs.host_views.len() as u32);
                    logs.host_views.len()
                ])
                .split(panes[1])
                .iter()
                .copied()
                .enumerate()
                .collect()
        }
    } else {
        Vec::new()
    };
    let notice = truncation_notice(logs);
    let reserved_notice_height = usize::from(notice.is_some()) + usize::from(logs.combined);
    let height = (host_areas
        .iter()
        .find_map(|(index, area)| (*index == lead).then_some(*area))
        .unwrap_or(panes[1])
        .height
        .saturating_sub(2) as usize)
        .saturating_sub(reserved_notice_height);
    logs.resize_content(usize::from(panes[1].width.saturating_sub(2)));
    logs.view_height = height;
    logs.scroll = logs.scroll.min(logs.matches.len().saturating_sub(height));
    let mut content: Vec<Line> = if logs.files_focus
        && !logs.files.is_empty()
        && visible_files.is_empty()
    {
        vec![Line::from(
            "No matching files. Clear the name filter with c or port filters with P.",
        )]
    } else if filter_active && !logs.rows.is_empty() && logs.matches.is_empty() {
        vec![Line::from(format!("Filtered by: '{}'", logs.search))]
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
            Line::from("o changes tracking mode; s checks on-demand files; d refreshes one file."),
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
                        " · older data omitted"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_else(|| "Cached content".into())
    };
    let title = if logs.combined {
        format!(
            "MERGED port {} · error + access · {} · m: unmerge",
            logs.selected_port().unwrap_or("unselected"),
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
        f.render_widget(content, panes[1]);
        logs.mouse_regions.push((panes[1], LogMouseTarget::Content));
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
                if logs.cache_is_partial(&view.key) {
                    " · older data omitted"
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
    let mut search_help = if logs.files_focus {
        vec![format!(
            "Ports: {}/{} shown · filter: {} · ports: {}",
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
            "{}{}{} · {} visible / {} lines · context -{} +{}{}",
            if filter_active {
                "Filtered by: '"
            } else {
                "Search: "
            },
            logs.search,
            if filter_active { "'" } else { "" },
            logs.matches.len(),
            logs.lines.len(),
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
    } else if logs.files_focus {
        search_help.push("←/→ or h/l: tracked columns".into());
        search_help.push("Shift+←/→ or H/L: all columns".into());
        search_help.push("Ctrl+←/→ or Ctrl+h/l: days".into());
        if logs.show_untracked {
            search_help.push("0-9: days ago".into());
        }
        search_help.extend([
            "F or /: filter".into(),
            "p: ports".into(),
            "P: clear ports".into(),
            "o: mode".into(),
            "u: untrack".into(),
            "c: clear filter".into(),
        ]);
    } else {
        search_help.extend(["/: edit".into(), "[/]: context".into(), "c: clear".into()]);
        if !host_areas.is_empty() {
            search_help.extend([
                "Tab/Shift-Tab: host focus".into(),
                "z: maximize/restore".into(),
            ]);
        }
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
    if !logs.file_filter_editing && !logs.search_editing && !logs.time_editing {
        if let Some(time) = logs.anchor_timestamp() {
            search_help.insert(0, format!("Time: {}", time.format("%Y-%m-%d %H:%M:%S%.3f")));
        }
    }
    f.render_widget(
        Paragraph::new(styled_menu_bar_items(&search_help))
            .style(Style::default().bg(menu_bar_background_color(terminal_focused))),
        chunks[2],
    );
    let message = if let Some(progress) = logs.activity_status() {
        progress
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
    let message = if logs.window.start.is_some() || logs.window.end.is_some() {
        format!("Window: {} · {}", logs.window_label(), message)
    } else {
        message
    };
    f.render_widget(
        Paragraph::new(message).style(Style::default().fg(Color::Cyan)),
        chunks[3],
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
            .min(usize::from(chunks[2].width.saturating_sub(1))) as u16;
        f.set_cursor_position((chunks[2].x + column, chunks[2].y));
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
            .position(|row| row.contains("ErrorLog.txt (1 host)"))
            .unwrap() as u16;
        let nonempty = rows
            .iter()
            .position(|row| row.contains("8000_ErrorLog.txt (1 host)"))
            .unwrap() as u16;
        let marker = rows[empty as usize].find("[D]").unwrap() as u16;
        assert_eq!(buffer[(marker, empty)].fg, Color::Gray);
        assert_eq!(buffer[(marker, nonempty)].fg, Color::Green);
        assert!(rows[empty as usize].contains("1 [D]"));
        assert!(rows[nonempty as usize].contains("2 [D]"));
        assert!(rows[empty as usize].contains("0 B n/a"));
        assert!(rows[nonempty as usize].contains("10 B n/a"));
        assert!(rows.iter().any(|row| row.contains("Files (2/2")));
        let title = rows
            .iter()
            .position(|row| row.contains("Files (2/2"))
            .unwrap() as u16;
        assert_eq!(buffer[(0, title)].fg, Color::Yellow);
        assert!(
            rows.iter()
                .any(|row| row.contains("L Live (5s)   D On demand (s)"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("P Partial (d) U Untracked (r)"))
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
            ("8000", "[D]", Color::Green),
            ("9000", "[U]", Color::Rgb(0, 0, 0)),
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
        assert!(screen.contains("Files: yesterday"));
        assert!(screen.contains("0-9: days ago"));
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
        assert!(first.contains("Files (1/1"));
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
        assert!(last.contains("Files (1/1"));
    }

    #[test]
    fn long_hostnames_are_readable_below_compact_file_details() {
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
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..50).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            rows.iter()
                .any(|row| row.contains("h1=taxtech-nonprod-ml1.nl.ema.pwcinternal.com"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("h6=taxtech-nonprod-ml6.nl.ema.pwcinternal.com"))
        );
        let file_row = rows
            .iter()
            .position(|row| row.contains("8000_ErrorLog.txt"))
            .unwrap();
        assert!(rows[file_row + 1].contains("h1"));
        assert!(rows[file_row + 1].contains("1.0 KiB"));
        assert!(rows[file_row + 1].contains("n/a"));
        assert!(!rows[file_row + 1].contains("taxtech"));
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
        assert_eq!(terminal.backend().buffer()[(1, 3)].fg, Color::LightRed);
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
        assert_eq!(logs.view_height, 25);
        assert!(logs.scroll > u16::MAX as usize);
        for (width, height) in [(60, 12), (20, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &mut logs, true)).unwrap();
        }
    }
}
