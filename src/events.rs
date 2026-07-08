use super::*;
use crate::ui::format_document_content;

pub(crate) fn handle_event(app: &mut App) -> Result<bool> {
    if event::poll(std::time::Duration::from_millis(100))? {
        if let Event::Key(key) = event::read()? {
            // Global quit
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                if let Err(e) = app.save_query_file_if_dirty() {
                    app.status_message = format!("Save error: {}", e);
                    return Ok(false);
                }
                return Ok(true);
            }

            if key.code == KeyCode::F(2) && key.modifiers.is_empty() {
                app.open_log_viewer();
                return Ok(false);
            }

            if handle_return_to_start_page_key(app, key)? {
                return Ok(false);
            }

            match app.mode {
                AppMode::StartPage => {}
                AppMode::FullScreenView => return handle_fullscreen_key(app, key),
                AppMode::LogViewer => return handle_log_viewer_key(app, key),
                AppMode::Interface(AppInterface::Servers) => {
                    return handle_servers_interface_key(app, key);
                }
                AppMode::ServerForm => return handle_server_form_key(app, key),
                AppMode::ServerDeleteConfirm => return handle_server_delete_confirm_key(app, key),
                AppMode::AppServerList => return handle_app_server_list_key(app, key),
                AppMode::AppServerForm => return handle_app_server_form_key(app, key),
                AppMode::AppServerDeleteConfirm => {
                    return handle_app_server_delete_confirm_key(app, key);
                }
                AppMode::CollectionSelect => return handle_collection_select_key(app, key),
                AppMode::DeleteConfirm => return handle_delete_confirm_key(app, key),
                AppMode::QueryFileSelect => return handle_query_file_select_key(app, key),
                AppMode::QueryFileCreate => return handle_query_file_create_key(app, key),
                AppMode::QueryFileRename => return handle_query_file_rename_key(app, key),
                AppMode::QueryFileDeleteConfirm => {
                    return handle_query_file_delete_confirm_key(app, key);
                }
                AppMode::ModuleCloneSelect => return handle_module_clone_select_key(app, key),
                AppMode::TrackedFolderSelect => return handle_tracked_folder_select_key(app, key),
                AppMode::TrackedFolderAdd => return handle_tracked_folder_add_key(app, key),
                AppMode::TrackedFolderDeleteConfirm => {
                    return handle_tracked_folder_delete_confirm_key(app, key);
                }
                AppMode::TrackedFolderCacheClearConfirm => {
                    return handle_tracked_folder_cache_clear_confirm_key(app, key);
                }
                AppMode::DocumentCreate => return handle_document_create_key(app, key),
                AppMode::DocumentMetadataEdit => {
                    return handle_document_metadata_edit_key(app, key);
                }
                AppMode::HelpOverlay => {
                    if key.code == KeyCode::Char('?') || key.code == KeyCode::Esc {
                        if let Some(prev) = app.previous_mode.take() {
                            app.mode = prev;
                        } else {
                            app.mode = AppMode::Normal;
                        }
                    }
                    return Ok(false);
                }
                AppMode::Normal => {}
            }

            // Global help overlay
            if key.code == KeyCode::Char('?') && key.modifiers.is_empty() {
                app.previous_mode = Some(app.mode.clone());
                app.mode = AppMode::HelpOverlay;
                return Ok(false);
            }

            if app.query_visible
                && key.code == KeyCode::Char('o')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                app.open_query_file_picker();
                return Ok(false);
            }

            // These Ctrl shortcuts remain active in both navigation and insert modes.
            if app.query_visible
                && key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                app.save_query_file_manually();
                return Ok(false);
            }

            if handle_query_insert_transition_key(app, key) {
                return Ok(false);
            }

            // Double-Esc in navigation mode: return to the centered start page
            if key.code == KeyCode::Esc {
                if app.edit_mode == EditMode::Navigate {
                    if app.mode != AppMode::StartPage {
                        if let Some(last) = app.last_esc {
                            if last.elapsed().as_millis() < 500 {
                                if let Err(e) = app.save_query_file_if_dirty() {
                                    app.status_message = format!("Save error: {}", e);
                                    return Ok(false);
                                }
                                app.last_esc = None;
                                app.enter_start_page();
                                return Ok(false);
                            }
                        }
                        app.last_esc = Some(Instant::now());
                    } else {
                        app.last_esc = None;
                    }
                } else {
                    app.last_esc = None;
                }
            } else {
                app.last_esc = None;
            }

            if app.edit_mode == EditMode::Navigate && handle_navigation_mode_key(app, key) {
                return Ok(false);
            }

            if app.edit_mode == EditMode::Navigate
                && app.focus != Focus::Command
                && app.focus != Focus::Filter
                && key.code == KeyCode::Char(':')
            {
                app.focus_command_panel();
                app.command_input = ":".to_string();
                app.update_autocomplete();
                return Ok(false);
            }

            match app.focus {
                Focus::Command => handle_command_key(app, key),
                Focus::Query => handle_query_key(app, key),
                Focus::Results => handle_results_key(app, key),
                Focus::Filter => handle_filter_key(app, key),
            }

            if app.should_quit {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn handle_return_to_start_page_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    if key.code != KeyCode::Char('q')
        || !key.modifiers.is_empty()
        || app.edit_mode != EditMode::Navigate
        || app.mode == AppMode::StartPage
    {
        return Ok(false);
    }

    let can_return = match app.mode {
        AppMode::Normal => !matches!(app.focus, Focus::Command | Focus::Filter),
        _ => false,
    };

    if !can_return {
        return Ok(false);
    }

    if let Err(e) = app.save_query_file_if_dirty() {
        app.status_message = format!("Save error: {}", e);
        return Ok(true);
    }
    app.enter_start_page();
    Ok(true)
}

pub(crate) fn handle_navigation_mode_key(app: &mut App, key: KeyEvent) -> bool {
    // Allow Shift through for BackTab; reject all other modifier combos
    let is_shift_only = key.modifiers == KeyModifiers::SHIFT;
    if !key.modifiers.is_empty() && !(key.code == KeyCode::BackTab && is_shift_only) {
        return false;
    }

    let is_navigation_surface = matches!(app.focus, Focus::Query | Focus::Results);

    match key.code {
        KeyCode::Char('1') if is_navigation_surface => {
            app.show_query_editor();
            app.edit_mode = EditMode::Navigate;
            true
        }
        KeyCode::Char('2') if is_navigation_surface => {
            app.focus_results_panel();
            true
        }
        KeyCode::Char('3') if is_navigation_surface => {
            app.focus_command_panel();
            app.command_input = ":".to_string();
            app.update_autocomplete();
            true
        }
        KeyCode::Char('\\') if is_navigation_surface => {
            app.toggle_file_list();
            true
        }
        KeyCode::Tab if is_navigation_surface && app.mode != AppMode::StartPage => {
            app.cycle_panel_focus();
            true
        }
        KeyCode::BackTab if is_navigation_surface && app.mode != AppMode::StartPage => {
            app.cycle_panel_focus_backward();
            true
        }
        KeyCode::Char('i') if app.focus == Focus::Query => {
            app.enter_insert_mode();
            true
        }
        KeyCode::Char('/') if app.focus == Focus::Results => {
            app.open_filter_input();
            true
        }
        _ => false,
    }
}

pub(crate) fn handle_query_insert_transition_key(app: &mut App, key: KeyEvent) -> bool {
    if app.edit_mode != EditMode::Insert || app.focus != Focus::Query {
        return false;
    }

    match key.code {
        KeyCode::Esc => {
            app.exit_insert_mode();
            true
        }
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.exit_insert_mode();
            app.cycle_panel_focus();
            true
        }
        _ => false,
    }
}

pub(crate) fn map_query_navigation_key(key: KeyEvent) -> Option<Input> {
    if !key.modifiers.is_empty() {
        return None;
    }

    match key.code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown => Some(Input::from(key)),
        KeyCode::Char('h') => Some(Input::from(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        ))),
        KeyCode::Char('j') => Some(Input::from(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ))),
        KeyCode::Char('k') => Some(Input::from(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))),
        KeyCode::Char('l') => Some(Input::from(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        ))),
        _ => None,
    }
}

pub(crate) fn handle_query_key(app: &mut App, key: KeyEvent) {
    // When edtui is active, handle app-level shortcuts then delegate to edtui
    if app.use_edtui {
        if key.code == KeyCode::F(5)
            || (key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT))
            || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            app.execute_query();
        } else if key.code == KeyCode::Char('c')
            && key.modifiers.is_empty()
            && app.edit_mode == EditMode::Navigate
        {
            app.open_module_clone_picker();
        } else if key.code == KeyCode::Char('\\') && key.modifiers.is_empty() {
            app.toggle_file_list();
        } else if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Err(e) = app.save_query_file_if_dirty() {
                app.status_message = format!("Save error: {}", e);
            }
        } else {
            let before = app.query_text();
            app.edtui_handler.on_key_event(key, &mut app.edtui_state);
            if app.query_text() != before {
                app.query_dirty = true;
                app.query_last_edit = Some(Instant::now());
            }
        }
        return;
    }

    if key.code == KeyCode::F(5) {
        app.execute_query();
    } else if app.edit_mode == EditMode::Navigate && key.modifiers.is_empty() {
        if app.file_list_visible {
            // File list navigation mode
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    let new_sel = app
                        .query_file_list_state
                        .selected()
                        .and_then(|sel| if sel > 0 { Some(sel - 1) } else { None });
                    if let Some(sel) = new_sel {
                        app.query_file_list_state.select(Some(sel));
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let new_sel = app.query_file_list_state.selected().and_then(|sel| {
                        if sel < app.query_files.len().saturating_sub(1) {
                            Some(sel + 1)
                        } else {
                            None
                        }
                    });
                    if let Some(sel) = new_sel {
                        app.query_file_list_state.select(Some(sel));
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(sel) = app.query_file_list_state.selected() {
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Char('e') => app.open_query_in_external_editor(),
                KeyCode::Char('c') => app.open_module_clone_picker(),
                KeyCode::Char('f') => app.open_tracked_folder_picker(),
                KeyCode::Char('n') => app.open_query_file_create(),
                KeyCode::Char('m') => app.start_rename_query_file(),
                KeyCode::Delete => {
                    if let Some(sel) = app.query_file_list_state.selected() {
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.file_delete_target = Some(path);
                            app.open_modal(AppMode::QueryFileDeleteConfirm);
                        }
                    }
                }
                KeyCode::Char('G') => {
                    if !app.query_files.is_empty() {
                        let last = app.query_files.len() - 1;
                        app.query_file_list_state.select(Some(last));
                        if let Some(path) = app.query_files.get(last).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Char('g') => {
                    if !app.query_files.is_empty() {
                        app.query_file_list_state.select(Some(0));
                        if let Some(path) = app.query_files.get(0).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Char('r') => app.execute_query(),
                KeyCode::Char('\\') => app.toggle_file_list(),
                _ => {
                    if let Some(input) = map_query_navigation_key(key) {
                        app.query_editor.input(input);
                    }
                }
            }
        } else {
            // Editor navigation mode (no file list visible)
            match key.code {
                KeyCode::Char('e') => app.open_query_in_external_editor(),
                KeyCode::Char('c') => app.open_module_clone_picker(),
                KeyCode::Char('o') => app.open_query_file_picker(),
                KeyCode::Char('n') => app.open_query_file_create(),
                KeyCode::Char('r') => app.execute_query(),
                KeyCode::Char('\\') => app.toggle_file_list(),
                _ => {
                    if let Some(input) = map_query_navigation_key(key) {
                        app.query_editor.input(input);
                    }
                }
            }
        }
    } else if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.execute_query();
    } else if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT))
        || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        app.execute_query();
    } else if app.edit_mode == EditMode::Insert {
        let before = app.query_editor.lines().join("\n");
        app.query_editor.input(Input::from(key));
        if app.query_editor.lines().join("\n") != before {
            app.query_dirty = true;
            app.query_last_edit = Some(Instant::now());
        }
    }
}

pub(crate) fn handle_command_key(app: &mut App, key: KeyEvent) {
    let is_start_page = app.mode == AppMode::StartPage;

    match key.code {
        KeyCode::Enter => {
            if !app.autocomplete_suggestions.is_empty() && app.command_input.starts_with(':') {
                // Accept the selected suggestion
                let suggestion = app.autocomplete_suggestions[app.autocomplete_selected];
                // Take just the command part (before any <arg> placeholder)
                let cmd_part = suggestion.split(' ').next().unwrap_or(suggestion);
                app.command_input = cmd_part.to_string();
                // If the command takes an argument, add a space
                if suggestion.contains('<') {
                    app.command_input.push(' ');
                    app.autocomplete_suggestions.clear();
                } else {
                    app.autocomplete_suggestions.clear();
                    app.execute_command();
                }
            } else {
                app.execute_command();
            }
        }
        KeyCode::Tab => {
            if app.command_input.is_empty() {
                if !is_start_page {
                    app.cycle_panel_focus();
                }
                return;
            }
            // Accept the current suggestion into the input
            if !app.autocomplete_suggestions.is_empty() {
                let suggestion = app.autocomplete_suggestions[app.autocomplete_selected];
                let cmd_part = suggestion.split(' ').next().unwrap_or(suggestion);
                app.command_input = cmd_part.to_string();
                if suggestion.contains('<') {
                    app.command_input.push(' ');
                }
                app.autocomplete_suggestions.clear();
            }
        }
        KeyCode::BackTab => {
            if app.command_input.is_empty() {
                if !is_start_page {
                    app.cycle_panel_focus_backward();
                }
                return;
            }
            if !app.autocomplete_suggestions.is_empty() {
                app.autocomplete_selected = if app.autocomplete_selected == 0 {
                    app.autocomplete_suggestions.len() - 1
                } else {
                    app.autocomplete_selected - 1
                };
            }
        }
        KeyCode::Up => {
            if !app.autocomplete_suggestions.is_empty() {
                app.autocomplete_selected = if app.autocomplete_selected == 0 {
                    app.autocomplete_suggestions.len() - 1
                } else {
                    app.autocomplete_selected - 1
                };
            }
        }
        KeyCode::Down => {
            if !app.autocomplete_suggestions.is_empty() {
                app.autocomplete_selected =
                    (app.autocomplete_selected + 1) % app.autocomplete_suggestions.len();
            }
        }
        KeyCode::Esc => {
            app.command_input.clear();
            app.autocomplete_suggestions.clear();
            app.autocomplete_selected = 0;
            if !is_start_page {
                app.cycle_panel_focus();
            }
        }
        KeyCode::Backspace => {
            app.command_input.pop();
            app.update_autocomplete();
        }
        KeyCode::Char(c) => {
            // '/' as first character switches to filter mode
            if c == '/' && app.command_input.is_empty() && !is_start_page {
                app.open_filter_input();
                return;
            }

            if c == ':' {
                if app.command_input == ":" {
                    return;
                }
                if app.command_input.is_empty() {
                    app.command_input.push(':');
                    app.update_autocomplete();
                    return;
                }
            } else if app.command_input.is_empty() {
                app.command_input.push(':');
            }

            app.command_input.push(c);
            app.update_autocomplete();
        }
        _ => {}
    }
}

pub(crate) fn handle_filter_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            // Apply filter and re-fetch
            let filter = app.filter_input.trim().to_string();
            if filter.is_empty() {
                app.uri_filter = None;
            } else {
                app.uri_filter = Some(filter);
            }
            app.current_page = 0;
            app.focus_results_panel();
            app.fetch_list();
        }
        KeyCode::Esc => {
            // Cancel filter editing
            app.focus_results_panel();
        }
        KeyCode::Backspace => {
            app.filter_input.pop();
        }
        KeyCode::Char(c) => {
            app.filter_input.push(c);
        }
        _ => {}
    }
}

pub(crate) fn handle_results_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if !app.records.is_empty() {
            app.delete_selected();
        }
        return;
    }
    if key.code == KeyCode::Char(':') {
        // handled globally now
        return;
    }

    // Determine which list we're navigating
    let is_query_results = !app.query_results.is_empty() && app.records.is_empty();
    let page_step = app.page_size.max(1);

    if is_query_results {
        if key.code != KeyCode::Char('g') {
            app.last_query_results_g = None;
        }

        match key.code {
            KeyCode::Char('d') => {
                if let Some(sel) = app.query_results_state.selected() {
                    let next = (sel + page_step).min(app.query_results.len().saturating_sub(1));
                    app.query_results_state.select(Some(next));
                }
                return;
            }
            KeyCode::Char('u') => {
                if let Some(sel) = app.query_results_state.selected() {
                    app.query_results_state
                        .select(Some(sel.saturating_sub(page_step)));
                }
                return;
            }
            KeyCode::Char('G') => {
                app.query_results_state
                    .select(Some(app.query_results.len().saturating_sub(1)));
                return;
            }
            KeyCode::Char('g') => {
                if let Some(last_g) = app.last_query_results_g {
                    if last_g.elapsed().as_millis() < 500 {
                        app.query_results_state.select(Some(0));
                        app.last_query_results_g = None;
                        return;
                    }
                }
                app.last_query_results_g = Some(Instant::now());
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if is_query_results {
                if let Some(sel) = app.query_results_state.selected() {
                    if sel > 0 {
                        app.query_results_state.select(Some(sel - 1));
                    }
                }
            } else {
                if let Some(sel) = app.list_state.selected() {
                    if sel > 0 {
                        app.list_state.select(Some(sel - 1));
                    }
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if is_query_results {
                if let Some(sel) = app.query_results_state.selected() {
                    if sel < app.query_results.len().saturating_sub(1) {
                        app.query_results_state.select(Some(sel + 1));
                    }
                }
            } else {
                if let Some(sel) = app.list_state.selected() {
                    if sel < app.records.len().saturating_sub(1) {
                        app.list_state.select(Some(sel + 1));
                    }
                }
            }
        }
        KeyCode::Char(' ') => {
            // Toggle selection (records only)
            if !is_query_results {
                if let Some(sel) = app.list_state.selected() {
                    if app.selected_indices.contains(&sel) {
                        app.selected_indices.retain(|&i| i != sel);
                    } else {
                        app.selected_indices.push(sel);
                    }
                }
            }
        }
        KeyCode::Enter => {
            if is_query_results {
                // Open full view of selected query result
                if let Some(sel) = app.query_results_state.selected() {
                    if let Some(result) = app.query_results.get(sel) {
                        app.active_document = None;
                        app.full_view_content = format_document_content(result);
                        app.full_view_scroll = 0;
                        app.mode = AppMode::FullScreenView;
                    }
                }
            } else {
                app.open_record();
            }
        }
        KeyCode::Tab => {
            if is_query_results && app.query_visible {
                app.focus_query_panel();
            }
        }
        KeyCode::Char('n') => {
            if !is_query_results {
                let max_page = app
                    .total_results
                    .map(|t| t.saturating_sub(1) / app.page_size)
                    .unwrap_or(usize::MAX);
                if app.current_page < max_page {
                    app.current_page += 1;
                    app.fetch_list();
                }
            }
        }
        KeyCode::Char('p') => {
            if !is_query_results {
                if app.current_page > 0 {
                    app.current_page -= 1;
                    app.fetch_list();
                }
            }
        }
        KeyCode::Char('c') if key.modifiers.is_empty() => {
            if !is_query_results {
                app.open_document_create();
            }
        }
        _ => {}
    }
}

pub(crate) fn handle_collection_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.collection_list_state.selected() {
                if sel > 0 {
                    app.collection_list_state.select(Some(sel - 1));
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.collection_list_state.selected() {
                if sel < app.collection_list.len().saturating_sub(1) {
                    app.collection_list_state.select(Some(sel + 1));
                }
            }
        }
        KeyCode::Enter => {
            if let Some(sel) = app.collection_list_state.selected() {
                if let Some(col) = app.collection_list.get(sel) {
                    app.current_collection = Some(col.clone());
                    app.current_page = 0;
                    app.close_modal_with_focus(Focus::Results);
                    app.fetch_list();
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_query_file_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.query_file_list_state.selected() {
                if sel > 0 {
                    app.query_file_list_state.select(Some(sel - 1));
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.query_file_list_state.selected() {
                if sel < app.query_files.len().saturating_sub(1) {
                    app.query_file_list_state.select(Some(sel + 1));
                }
            }
        }
        KeyCode::Enter => {
            if let Some(sel) = app.query_file_list_state.selected() {
                if let Some(path) = app.query_files.get(sel).cloned() {
                    app.close_modal_with_focus(Focus::Query);
                    app.select_query_file(path);
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_module_clone_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_module_clone_selection(-1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_module_clone_selection(1);
        }
        KeyCode::Tab => {
            app.move_module_clone_selection(1);
        }
        KeyCode::BackTab => {
            app.move_module_clone_selection(-1);
        }
        KeyCode::Enter => {
            app.clone_selected_module();
        }
        KeyCode::Backspace => {
            app.module_clone_filter_input.pop();
            app.rebuild_module_clone_filtered_uris();
        }
        KeyCode::Char(c) if key.modifiers.is_empty() => {
            app.module_clone_filter_input.push(c);
            app.rebuild_module_clone_filtered_uris();
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_tracked_folder_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_tracked_folder_selection(-1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_tracked_folder_selection(1);
        }
        KeyCode::Char('g') => {
            app.tracked_folder_list_state
                .select(app.first_tracked_folder_item_index());
        }
        KeyCode::Char('G') => {
            app.tracked_folder_list_state
                .select(app.last_tracked_folder_item_index());
        }
        KeyCode::Enter => {
            app.switch_to_selected_tracked_folder();
        }
        KeyCode::Char('a') => {
            app.open_tracked_folder_add();
        }
        KeyCode::Char('f') => {
            app.toggle_selected_tracked_folder_favorite();
        }
        KeyCode::Char('d') => {
            app.start_delete_selected_tracked_folder();
        }
        KeyCode::Char('c') => {
            app.start_clear_selected_tracked_folder_cache();
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_tracked_folder_add_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.open_modal(AppMode::TrackedFolderSelect);
        }
        KeyCode::Enter => {
            app.add_tracked_folder_from_input();
        }
        KeyCode::Backspace => {
            app.tracked_folder_input.pop();
        }
        KeyCode::Char(c) => {
            app.tracked_folder_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_tracked_folder_delete_confirm_key(
    app: &mut App,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_delete_selected_tracked_folder();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.tracked_folder_delete_target = None;
            app.mode = AppMode::TrackedFolderSelect;
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_tracked_folder_cache_clear_confirm_key(
    app: &mut App,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_clear_selected_tracked_folder_cache();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.tracked_folder_cache_clear_target = None;
            app.mode = AppMode::TrackedFolderSelect;
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_query_file_create_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Enter => {
            app.create_new_query_file_from_input();
        }
        KeyCode::Backspace => {
            app.new_query_file_input.pop();
        }
        KeyCode::Char(c) => {
            app.new_query_file_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_delete_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_delete();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_fullscreen_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let half_page = app.fullscreen_area_height.saturating_sub(2) / 2;
    let total_lines = app.full_view_content.lines().count() as u16;
    let visible_lines = app.fullscreen_area_height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(visible_lines);

    if key.code != KeyCode::Char('g') {
        app.last_fullscreen_g = None;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Char('?') if key.modifiers.is_empty() => {
            app.previous_mode = Some(app.mode.clone());
            app.mode = AppMode::HelpOverlay;
        }
        KeyCode::Char('e') if key.modifiers.is_empty() => {
            app.open_document_in_external_editor();
        }
        KeyCode::Char('m') if key.modifiers.is_empty() => {
            app.open_document_metadata_edit();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.full_view_scroll = (app.full_view_scroll + 1).min(max_scroll);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(1);
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.full_view_scroll = (app.full_view_scroll + 20).min(max_scroll);
        }
        KeyCode::PageUp => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(20);
        }
        KeyCode::Char('d') => {
            app.full_view_scroll = (app.full_view_scroll + half_page.max(1)).min(max_scroll);
        }
        KeyCode::Char('u') => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(half_page.max(1));
        }
        KeyCode::Char('G') => {
            app.full_view_scroll = max_scroll;
        }
        KeyCode::Char('g') => {
            if let Some(last_g) = app.last_fullscreen_g {
                if last_g.elapsed().as_millis() < 500 {
                    app.full_view_scroll = 0;
                    app.last_fullscreen_g = None;
                    return Ok(false);
                }
            }
            app.last_fullscreen_g = Some(Instant::now());
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_log_viewer_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let half_page = app.log_viewer_area_height.saturating_sub(2) / 2;
    let max_scroll = app.log_viewer_max_scroll();

    if key.code != KeyCode::Char('g') {
        app.last_log_viewer_g = None;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close_log_viewer();
        }
        KeyCode::Char('?') if key.modifiers.is_empty() => {
            app.previous_mode = Some(app.mode.clone());
            app.mode = AppMode::HelpOverlay;
        }
        KeyCode::Char('e') if key.modifiers.is_empty() => {
            app.open_log_in_external_editor();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.log_view_scroll = (app.log_view_scroll + 1).min(max_scroll);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.log_view_scroll = app.log_view_scroll.saturating_sub(1);
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.log_view_scroll = (app.log_view_scroll + 20).min(max_scroll);
        }
        KeyCode::PageUp => {
            app.log_view_scroll = app.log_view_scroll.saturating_sub(20);
        }
        KeyCode::Char('d') => {
            app.log_view_scroll = (app.log_view_scroll + half_page.max(1)).min(max_scroll);
        }
        KeyCode::Char('u') => {
            app.log_view_scroll = app.log_view_scroll.saturating_sub(half_page.max(1));
        }
        KeyCode::Char('G') => {
            app.log_view_scroll = max_scroll;
        }
        KeyCode::Char('g') => {
            if let Some(last_g) = app.last_log_viewer_g {
                if last_g.elapsed().as_millis() < 500 {
                    app.log_view_scroll = 0;
                    app.last_log_viewer_g = None;
                    return Ok(false);
                }
            }
            app.last_log_viewer_g = Some(Instant::now());
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_servers_interface_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close_interface_restore_focus();
        }
        KeyCode::Tab => {
            app.servers_interface_focus = match app.servers_interface_focus {
                ServersInterfaceFocus::Servers => ServersInterfaceFocus::AppServers,
                ServersInterfaceFocus::AppServers => ServersInterfaceFocus::Servers,
            };
        }
        KeyCode::Up | KeyCode::Char('k') => match app.servers_interface_focus {
            ServersInterfaceFocus::Servers => app.move_server_selection(-1),
            ServersInterfaceFocus::AppServers => app.move_app_server_selection(-1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.servers_interface_focus {
            ServersInterfaceFocus::Servers => app.move_server_selection(1),
            ServersInterfaceFocus::AppServers => app.move_app_server_selection(1),
        },
        KeyCode::Char('g') => match app.servers_interface_focus {
            ServersInterfaceFocus::Servers => {
                if !app.config.servers.is_empty() {
                    app.server_list_state.select(Some(0));
                }
            }
            ServersInterfaceFocus::AppServers => {
                if !app.app_server_list.is_empty() {
                    app.app_server_list_state.select(Some(0));
                }
            }
        },
        KeyCode::Char('G') => match app.servers_interface_focus {
            ServersInterfaceFocus::Servers => {
                if !app.config.servers.is_empty() {
                    app.server_list_state
                        .select(Some(app.config.servers.len().saturating_sub(1)));
                }
            }
            ServersInterfaceFocus::AppServers => {
                if !app.app_server_list.is_empty() {
                    app.app_server_list_state
                        .select(Some(app.app_server_list.len().saturating_sub(1)));
                }
            }
        },
        KeyCode::Enter => match app.servers_interface_focus {
            ServersInterfaceFocus::Servers => app.activate_selected_server(),
            ServersInterfaceFocus::AppServers => app.activate_selected_app_server(),
        },
        KeyCode::Char('a') => {
            if app.servers_interface_focus == ServersInterfaceFocus::Servers {
                app.open_server_add();
            }
        }
        KeyCode::Char('e') => {
            if app.servers_interface_focus == ServersInterfaceFocus::Servers {
                app.open_server_edit();
            }
        }
        KeyCode::Char('d') => {
            if app.servers_interface_focus == ServersInterfaceFocus::Servers {
                app.start_delete_selected_server();
            }
        }
        KeyCode::Char('r') => {
            if app.servers_interface_focus == ServersInterfaceFocus::AppServers {
                app.refresh_app_server_list_for_interface();
            }
        }
        KeyCode::Char('p') => {
            if app.servers_interface_focus == ServersInterfaceFocus::AppServers {
                app.open_app_server_list();
            }
        }
        KeyCode::Char('?') if key.modifiers.is_empty() => {
            app.previous_mode = Some(app.mode.clone());
            app.mode = AppMode::HelpOverlay;
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_server_form_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.server_edit_target = None;
            app.mode = AppMode::Interface(AppInterface::Servers);
        }
        KeyCode::Enter => {
            app.submit_server_form();
        }
        KeyCode::Tab => {
            app.server_form_step = (app.server_form_step + 1) % 6;
        }
        KeyCode::BackTab => {
            app.server_form_step = if app.server_form_step == 0 {
                5
            } else {
                app.server_form_step - 1
            };
        }
        KeyCode::Up => {
            if app.server_form_step == 4 {
                app.cycle_auth_type_prev();
            } else if app.server_form_step == 5 {
                app.cycle_insecure_flag();
            }
        }
        KeyCode::Down => {
            if app.server_form_step == 4 {
                app.cycle_auth_type_next();
            } else if app.server_form_step == 5 {
                app.cycle_insecure_flag();
            }
        }
        KeyCode::Backspace => {
            app.server_form_fields[app.server_form_step].pop();
        }
        KeyCode::Char(c) => {
            app.server_form_fields[app.server_form_step].push(c);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_server_delete_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_delete_selected_server();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.server_delete_target = None;
            app.mode = AppMode::Interface(AppInterface::Servers);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_query_file_rename_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
            app.rename_file_target = None;
        }
        KeyCode::Enter => {
            app.rename_query_file();
        }
        KeyCode::Backspace => {
            app.rename_file_input.pop();
        }
        KeyCode::Char(c) => {
            app.rename_file_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_query_file_delete_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Some(path) = app.file_delete_target.take() {
                app.delete_query_file(path);
            }
            app.close_modal_with_focus(Focus::Query);
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.file_delete_target = None;
            app.close_modal_restore_focus();
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_document_create_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = app.previous_mode.clone().unwrap_or(AppMode::Normal);
            app.previous_mode = None;
        }
        KeyCode::Enter => {
            app.submit_document_create();
        }
        KeyCode::Tab => {
            app.doc_form_step = (app.doc_form_step + 1) % 4;
        }
        KeyCode::Backspace => {
            match app.doc_form_step {
                0 => app.doc_form_uri.pop(),
                1 => app.doc_form_collections.pop(),
                2 => app.doc_form_quality.pop(),
                3 => app.doc_form_content.pop(),
                _ => None,
            };
        }
        KeyCode::Char(c) => match app.doc_form_step {
            0 => app.doc_form_uri.push(c),
            1 => app.doc_form_collections.push(c),
            2 => app.doc_form_quality.push(c),
            3 => app.doc_form_content.push(c),
            _ => {}
        },
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_document_metadata_edit_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = app.previous_mode.clone().unwrap_or(AppMode::FullScreenView);
            app.previous_mode = None;
        }
        KeyCode::Enter => {
            app.submit_document_metadata_edit();
        }
        KeyCode::Tab => {
            app.doc_form_step = (app.doc_form_step + 1) % 4;
        }
        KeyCode::Backspace => {
            match app.doc_form_step {
                0 => app.doc_form_uri.pop(),
                1 => app.doc_form_collections.pop(),
                2 => app.doc_form_quality.pop(),
                3 => app.doc_form_content.pop(),
                _ => None,
            };
        }
        KeyCode::Char(c) => match app.doc_form_step {
            0 => app.doc_form_uri.push(c),
            1 => app.doc_form_collections.push(c),
            2 => app.doc_form_quality.push(c),
            3 => app.doc_form_content.push(c),
            _ => {}
        },
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_app_server_list_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = AppMode::Interface(AppInterface::Servers);
        }
        KeyCode::Char('a') => {
            app.open_app_server_add();
        }
        KeyCode::Char('e') => {
            app.open_app_server_edit();
        }
        KeyCode::Char('d') => {
            let Some(selected) = app.app_server_list_state.selected() else {
                app.status_message = "No app server selected.".to_string();
                return Ok(false);
            };
            let Some(endpoint) = app.app_server_list.get(selected) else {
                app.status_message = "No app server selected.".to_string();
                return Ok(false);
            };
            app.app_server_delete_target = Some(endpoint.name.clone());
            app.mode = AppMode::AppServerDeleteConfirm;
        }
        KeyCode::Char('r') => {
            app.refresh_app_servers_from_discovery();
        }
        KeyCode::Enter => {
            app.activate_selected_app_server();
        }
        KeyCode::Up => {
            if app.app_server_list.is_empty() {
                app.app_server_list_state.select(None);
            } else {
                let current = app.app_server_list_state.selected().unwrap_or(0);
                app.app_server_list_state
                    .select(Some(current.saturating_sub(1)));
            }
        }
        KeyCode::Down => {
            if app.app_server_list.is_empty() {
                app.app_server_list_state.select(None);
            } else {
                let current = app.app_server_list_state.selected().unwrap_or(0);
                let next = (current + 1).min(app.app_server_list.len().saturating_sub(1));
                app.app_server_list_state.select(Some(next));
            }
        }
        KeyCode::Home => {
            if !app.app_server_list.is_empty() {
                app.app_server_list_state.select(Some(0));
            }
        }
        KeyCode::End => {
            if !app.app_server_list.is_empty() {
                app.app_server_list_state
                    .select(Some(app.app_server_list.len().saturating_sub(1)));
            }
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_app_server_form_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::AppServerList;
        }
        KeyCode::Enter => {
            app.submit_app_server_form();
        }
        KeyCode::Tab => {
            app.app_server_form_step = (app.app_server_form_step + 1) % 5;
        }
        KeyCode::BackTab => {
            app.app_server_form_step = if app.app_server_form_step == 0 {
                4
            } else {
                app.app_server_form_step - 1
            };
        }
        KeyCode::Backspace => {
            app.app_server_form_fields[app.app_server_form_step].pop();
        }
        KeyCode::Char(c) => {
            app.app_server_form_fields[app.app_server_form_step].push(c);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_app_server_delete_confirm_key(
    app: &mut App,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.delete_app_server();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.app_server_delete_target = None;
            app.mode = AppMode::AppServerList;
        }
        _ => {}
    }
    Ok(false)
}
