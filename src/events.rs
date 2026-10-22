use super::*;
use crate::ui::format_document_content;

pub(crate) fn handle_event(app: &mut App) -> Result<bool> {
    if event::poll(std::time::Duration::from_millis(100))? {
        if let Event::Key(key) = event::read()? {
            return handle_key_event(app, key);
        }
    }
    Ok(false)
}

pub(crate) fn handle_key_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    {
        {
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

            // Global help overlay: reachable from every mode except text-input
            // forms (where `?` is typed literally) and the overlay itself.
            if key.code == KeyCode::Char('?')
                && key.modifiers.is_empty()
                && app.mode != AppMode::HelpOverlay
                && !app.help_key_types_text()
            {
                app.open_help_overlay();
                return Ok(false);
            }

            match app.mode {
                AppMode::StartPage => {}
                AppMode::FullScreenView => return handle_fullscreen_key(app, key),
                AppMode::LogViewer => return handle_log_viewer_key(app, key),
                AppMode::MarkLogicLogs => return handle_server_logs_key(app, key),
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
                AppMode::QueryResultFormatSelect => {
                    return handle_query_result_format_select_key(app, key);
                }
                AppMode::ActiveVariables => return handle_active_variables_key(app, key),
                AppMode::DeclaredVariables => return handle_declared_variables_key(app, key),
                AppMode::MultiRunForm => return handle_multi_run_form_key(app, key),
                AppMode::ModuleCloneSelect => return handle_module_clone_select_key(app, key),
                AppMode::ModuleCreate => return handle_module_create_key(app, key),
                AppMode::ModuleResetConfirm => return handle_module_reset_confirm_key(app, key),
                AppMode::ModuleDropConfirm => return handle_module_drop_confirm_key(app, key),
                AppMode::ModuleDiffView => return handle_module_diff_view_key(app, key),
                AppMode::ModuleDropCacheConfirm => {
                    return handle_module_drop_cache_confirm_key(app, key);
                }
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
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.help_scroll = app.help_scroll.saturating_add(1)
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.help_scroll = app.help_scroll.saturating_sub(1)
                        }
                        KeyCode::PageDown | KeyCode::Char('d') => {
                            app.help_scroll = app.help_scroll.saturating_add(10)
                        }
                        KeyCode::PageUp | KeyCode::Char('u') => {
                            app.help_scroll = app.help_scroll.saturating_sub(10)
                        }
                        _ => {}
                    }
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

            // Global help overlay is handled before the mode dispatch above.

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
            app.set_edit_mode(EditMode::Navigate);
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
    if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.open_declared_variables();
        return;
    }
    if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.open_active_variables();
        return;
    }
    // External editing is available regardless of the inline editor's mode.
    if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.open_query_in_external_editor();
        return;
    }
    if app.use_edtui {
        handle_edtui_query_key(app, key);
        return;
    }

    if key.code == KeyCode::F(5) {
        app.execute_query();
    } else if app.edit_mode == EditMode::Navigate
        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
    {
        if key.code == KeyCode::Char('T') {
            app.toggle_modules_view();
            return;
        }

        if app.modules_view && app.handle_modules_sidebox_key(key) {
            return;
        }

        if key.code == KeyCode::Char('M')
            && key.modifiers == KeyModifiers::SHIFT
            && app.file_list_visible
        {
            app.start_rename_query_file();
            return;
        }

        if app.file_list_visible {
            if !handle_query_file_list_key(app, key)
                && let Some(input) = map_query_navigation_key(key)
            {
                app.query_editor.input(input);
            }
        } else if key.code == KeyCode::Char('o') {
            app.open_query_file_picker();
        } else if !handle_query_command_key(app, key)
            && let Some(input) = map_query_navigation_key(key)
        {
            app.query_editor.input(input);
        }
    } else if is_run_shortcut(&key) {
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

pub(crate) fn handle_active_variables_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let field_count = app.active_variable_fields.len().saturating_mul(2);
    match key.code {
        KeyCode::Esc => app.close_modal_restore_focus(),
        KeyCode::Enter => app.save_active_variables(),
        KeyCode::Tab if field_count > 0 => {
            app.active_variable_step = (app.active_variable_step + 1) % field_count;
        }
        KeyCode::BackTab if field_count > 0 => {
            app.active_variable_step = (app.active_variable_step + field_count - 1) % field_count;
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.active_variable_fields
                .push((String::new(), String::new()));
            app.active_variable_step = app.active_variable_fields.len().saturating_sub(1) * 2;
        }
        KeyCode::Delete if !app.active_variable_fields.is_empty() => {
            let row = app.active_variable_step / 2;
            app.active_variable_fields.remove(row);
            if app.active_variable_fields.is_empty() {
                app.active_variable_fields
                    .push((String::new(), String::new()));
            }
            app.active_variable_step = (row.min(app.active_variable_fields.len() - 1)) * 2;
        }
        KeyCode::Backspace => {
            if let Some((name, value)) = app
                .active_variable_fields
                .get_mut(app.active_variable_step / 2)
            {
                if app.active_variable_step % 2 == 0 {
                    name.pop();
                } else {
                    value.pop();
                }
            }
        }
        KeyCode::Char(ch) => {
            if let Some((name, value)) = app
                .active_variable_fields
                .get_mut(app.active_variable_step / 2)
            {
                if app.active_variable_step % 2 == 0 {
                    name.push(ch);
                } else {
                    value.push(ch);
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_declared_variables_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let field_count = app.declared_variable_fields.len();
    match key.code {
        KeyCode::Esc => app.close_modal_restore_focus(),
        KeyCode::Enter => app.submit_declared_variables(),
        KeyCode::Tab if field_count > 0 => {
            app.declared_variable_step = (app.declared_variable_step + 1) % field_count;
        }
        KeyCode::BackTab if field_count > 0 => {
            app.declared_variable_step =
                (app.declared_variable_step + field_count - 1) % field_count;
        }
        KeyCode::Backspace => {
            if let Some((_, value)) = app
                .declared_variable_fields
                .get_mut(app.declared_variable_step)
            {
                value.pop();
            }
        }
        KeyCode::Char(ch) => {
            if let Some((_, value)) = app
                .declared_variable_fields
                .get_mut(app.declared_variable_step)
            {
                value.push(ch);
            }
        }
        _ => {}
    }
    Ok(false)
}

/// Query panel keys when edtui is the editor.
///
/// In Normal mode (`edit_mode == Navigate`) the Files pane and app commands
/// take their keys. In Insert mode every key goes to the editor, except the
/// run shortcuts, which work in both modes.
fn handle_edtui_query_key(app: &mut App, key: KeyEvent) {
    if is_run_shortcut(&key) {
        app.execute_query();
        return;
    }
    if app.edit_mode == EditMode::Navigate && handle_query_normal_mode_key(app, key) {
        return;
    }

    let before = app.query_text();
    app.edtui_handler.on_key_event(key, &mut app.edtui_state);
    app.sync_edit_mode_from_editor();
    if app.query_text() != before {
        app.query_dirty = true;
        app.query_last_edit = Some(Instant::now());
    }
}

fn is_run_shortcut(key: &KeyEvent) -> bool {
    key.code == KeyCode::F(5)
        || (key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL))
}

/// Normal-mode keys for the edtui editor: the modules sidebox, the Files pane,
/// then app commands. Returns false when the key belongs to the editor.
fn handle_query_normal_mode_key(app: &mut App, key: KeyEvent) -> bool {
    if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
        return false;
    }
    if key.code == KeyCode::Char('T') {
        app.toggle_modules_view();
        return true;
    }
    if app.modules_view && app.handle_modules_sidebox_key(key) {
        return true;
    }
    if app.file_list_visible && handle_query_file_list_key(app, key) {
        return true;
    }
    handle_query_command_key(app, key)
}

/// Files pane keys in navigate mode. Returns false for keys it does not use.
fn handle_query_file_list_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.query_file_list_state.selected()
                && sel > 0
            {
                app.open_query_row(sel - 1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let row_count = app.query_file_rows().len();
            if let Some(sel) = app.query_file_list_state.selected()
                && sel + 1 < row_count
            {
                app.open_query_row(sel + 1);
            }
        }
        KeyCode::Enter => match app.selected_query_row() {
            Some(row) if row.is_dir => app.toggle_query_folder(&row.path),
            Some(row) => app.select_query_file(row.path),
            None => {}
        },
        KeyCode::Char('e') => app.open_query_in_external_editor(),
        KeyCode::Char('p') => app.open_compiled_query_preview(),
        KeyCode::Char('m') => app.open_module_picker(),
        KeyCode::Char('f') => app.open_tracked_folder_picker(),
        KeyCode::Char('n') => app.open_query_file_create(),
        KeyCode::Char('s') if app.active_sync_info().is_some() => app.sync_active_module(),
        KeyCode::Char('t') if app.active_sync_info().is_some() => app.reset_remote_module(),
        KeyCode::Char('d') if app.active_sync_info().is_some() => app.drop_local_module(),
        KeyCode::Delete => match app.selected_query_row() {
            Some(row) if row.is_dir => app.delete_query_folder(row.path),
            Some(row) => {
                app.file_delete_target = Some(row.path);
                app.open_modal(AppMode::QueryFileDeleteConfirm);
            }
            None => {}
        },
        KeyCode::Char('G') => {
            let row_count = app.query_file_rows().len();
            if row_count > 0 {
                app.open_query_row(row_count - 1);
            }
        }
        KeyCode::Char('g') => {
            if !app.query_file_rows().is_empty() {
                app.open_query_row(0);
            }
        }
        KeyCode::Char('M') => app.start_rename_query_file(),
        KeyCode::Char('r') => app.execute_query(),
        KeyCode::Char('R') => app.open_multi_run(),
        KeyCode::Char('X') => app.cancel_running_query(),
        KeyCode::Char('\\') => app.toggle_file_list(),
        _ => return false,
    }
    true
}

/// App commands in navigate mode when the Files pane is hidden. Returns false
/// for keys it does not use.
fn handle_query_command_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('e') => app.open_query_in_external_editor(),
        KeyCode::Char('p') => app.open_compiled_query_preview(),
        KeyCode::Char('m') => app.open_module_picker(),
        KeyCode::Char('n') => app.open_query_file_create(),
        KeyCode::Char('s') if app.active_sync_info().is_some() => app.sync_active_module(),
        KeyCode::Char('t') if app.active_sync_info().is_some() => app.reset_remote_module(),
        KeyCode::Char('d') if app.active_sync_info().is_some() => app.drop_local_module(),
        KeyCode::Char('r') => app.execute_query(),
        KeyCode::Char('R') => app.open_multi_run(),
        KeyCode::Char('X') => app.cancel_running_query(),
        KeyCode::Char('\\') => app.toggle_file_list(),
        _ => return false,
    }
    true
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
            let filter = app.filter_input.trim().to_string();
            if app.filtering_query_results() {
                app.apply_query_results_filter(filter);
                app.focus_results_panel();
            } else {
                // Apply a document-URI filter and re-fetch.
                if filter.is_empty() {
                    app.uri_filter = None;
                } else {
                    app.uri_filter = Some(filter);
                }
                app.current_page = 0;
                app.focus_results_panel();
                app.fetch_list();
            }
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
        if app.query_results_format == QueryResultsFormat::Uris && !app.query_results.is_empty() {
            app.delete_selected_uri_query_result();
        } else if !app.records.is_empty() {
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
            KeyCode::Char('F')
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                app.open_query_results_format_picker();
                return;
            }
            KeyCode::Char('d') => {
                if let Some(sel) = app.query_results_state.selected() {
                    let next =
                        (sel + page_step).min(app.visible_query_result_count().saturating_sub(1));
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
                let visible = app.visible_query_result_count();
                app.query_results_state
                    .select((visible > 0).then_some(visible - 1));
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
                    if sel < app.visible_query_result_count().saturating_sub(1) {
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
                if app.query_results_format == QueryResultsFormat::Uris {
                    app.open_uri_query_result();
                } else {
                    // Open full view of selected query result
                    if let Some(sel) = app.query_results_state.selected() {
                        if let Some(result) = app
                            .visible_query_result_indices()
                            .get(sel)
                            .and_then(|index| app.query_results.get(*index))
                        {
                            app.active_document = None;
                            app.full_view_content = format_document_content(result);
                            app.full_view_scroll = 0;
                            app.mode = AppMode::FullScreenView;
                        }
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
        KeyCode::Char('X') if key.modifiers == KeyModifiers::SHIFT => {
            app.cancel_running_query();
        }
        _ => {}
    }
}

pub(crate) fn handle_query_result_format_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    const OPTION_COUNT: usize = QueryResultsFormat::ALL.len() + 1; // + edit action
    match key.code {
        KeyCode::Esc => app.close_modal_restore_focus(),
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(selected) = app.query_results_format_picker_state.selected()
                && selected > 0
            {
                app.query_results_format_picker_state
                    .select(Some(selected - 1));
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(selected) = app.query_results_format_picker_state.selected()
                && selected + 1 < OPTION_COUNT
            {
                app.query_results_format_picker_state
                    .select(Some(selected + 1));
            }
        }
        KeyCode::Enter => app.select_query_results_format(),
        _ => {}
    }
    Ok(false)
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
            if let Some(sel) = app.query_file_list_state.selected()
                && sel > 0
            {
                app.set_query_row_index(sel - 1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let row_count = app.query_file_rows().len();
            if let Some(sel) = app.query_file_list_state.selected()
                && sel + 1 < row_count
            {
                app.set_query_row_index(sel + 1);
            }
        }
        KeyCode::Enter => match app.selected_query_row() {
            Some(row) if row.is_dir => app.toggle_query_folder(&row.path),
            Some(row) => {
                app.close_modal_with_focus(Focus::Query);
                app.select_query_file(row.path);
            }
            None => {}
        },
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_module_clone_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        _ if app.module_search.focus_input => match key.code {
            KeyCode::Enter => {
                app.module_search.page = 0;
                app.search_modules();
            }
            KeyCode::Backspace => {
                app.module_search.pattern.pop();
            }
            KeyCode::Char(c) => {
                app.module_search.pattern.push(c);
            }
            KeyCode::Tab => {
                app.module_search.focus_input = false;
                if app.module_search.selected.selected().is_none()
                    && !app.module_search.results.is_empty()
                {
                    app.module_search.selected.select(Some(0));
                }
            }
            _ => {}
        },
        _ => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.move_module_search_selection(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.move_module_search_selection(1);
            }
            KeyCode::Tab => {
                app.module_search.focus_input = true;
            }
            KeyCode::Enter => {
                app.checkout_selected_module();
            }
            KeyCode::Char('S') => {
                app.save_marked_modules();
            }
            KeyCode::Char(' ') => {
                // Toggle the mark on the highlighted module; marks persist
                // across searches until saved (Enter/S) or cleared.
                app.toggle_mark_highlighted_module();
            }
            KeyCode::Char('a') => {
                app.mark_all_visible_modules();
            }
            KeyCode::Char('u') => {
                app.clear_marked_modules();
            }
            KeyCode::Char('n') => {
                app.fetch_module_search_page(1);
            }
            KeyCode::Char('p') => {
                app.fetch_module_search_page(-1);
            }
            KeyCode::Char('g') => {
                if !app.module_search.results.is_empty() {
                    app.module_search.selected.select(Some(0));
                }
            }
            KeyCode::Char('G') => {
                if !app.module_search.results.is_empty() {
                    let last = app.module_search.results.len() - 1;
                    app.module_search.selected.select(Some(last));
                }
            }
            _ => {}
        },
    }
    Ok(false)
}

pub(crate) fn handle_module_reset_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_module_reset_drop_local(false);
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            app.confirm_module_reset_drop_local(true);
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_module_drop_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_module_drop(app.module_drop_needs_reset);
        }
        KeyCode::Char('n') | KeyCode::Char('N') => {
            app.confirm_module_drop(false);
        }
        KeyCode::Esc => {
            app.module_drop_needs_reset = false;
            app.close_modal_restore_focus();
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_module_diff_view_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let total = app.module_diff_lines.len() as u16;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close_modal_restore_focus();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.module_diff_scroll = app.module_diff_scroll.saturating_add(1).min(total);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.module_diff_scroll = app.module_diff_scroll.saturating_sub(1);
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.module_diff_scroll = app.module_diff_scroll.saturating_add(20).min(total);
        }
        KeyCode::PageUp => {
            app.module_diff_scroll = app.module_diff_scroll.saturating_sub(20);
        }
        KeyCode::Char('G') => {
            app.module_diff_scroll = total;
        }
        KeyCode::Char('g') => {
            app.module_diff_scroll = 0;
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_module_create_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Enter => {
            app.create_module_from_input();
        }
        KeyCode::Backspace => {
            app.new_module_uri_input.pop();
        }
        KeyCode::Char(c) => {
            app.new_module_uri_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) fn handle_module_drop_cache_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_drop_cached_selected();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.module_drop_target = None;
            app.close_modal_restore_focus();
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

pub(crate) fn handle_multi_run_form_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Enter => {
            app.submit_multi_run_form();
        }
        KeyCode::Tab => {
            app.multi_run_form_step = (app.multi_run_form_step + 1) % 2;
        }
        KeyCode::BackTab => {
            app.multi_run_form_step = if app.multi_run_form_step == 0 { 1 } else { 0 };
        }
        KeyCode::Backspace => {
            if let Some(field) = app.multi_run_form_fields.get_mut(app.multi_run_form_step) {
                field.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(field) = app.multi_run_form_fields.get_mut(app.multi_run_form_step) {
                field.push(c);
            }
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
            app.query_uri_delete_target = None;
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

pub(crate) fn handle_app_server_delete_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
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

pub(crate) fn handle_server_logs_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let logs = &mut app.server_logs;
    if logs.options_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => logs.options_open = false,
            KeyCode::Up | KeyCode::Char('k') => {
                logs.option_selected = logs.option_selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                logs.option_selected =
                    (logs.option_selected + 1).min(logs.option_items().len().saturating_sub(1))
            }
            KeyCode::Enter | KeyCode::Char(' ') => logs.change_option(),
            _ => {}
        }
        return Ok(false);
    }
    if logs.time_editing {
        match key.code {
            KeyCode::Esc => logs.time_editing = false,
            KeyCode::Enter => logs.seek_time(),
            KeyCode::Backspace => {
                logs.time_input.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                logs.time_input.push(c)
            }
            _ => {}
        }
        return Ok(false);
    }
    if logs.file_filter_editing {
        match key.code {
            KeyCode::Esc => {
                logs.clear_file_filter();
                logs.file_filter_editing = false;
            }
            KeyCode::Enter => logs.file_filter_editing = false,
            KeyCode::Backspace => logs.edit_file_filter(None, true),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                logs.edit_file_filter(Some(c), false)
            }
            _ => {}
        }
        return Ok(false);
    }
    if logs.search_editing {
        match key.code {
            KeyCode::Esc => {
                // Escape should get the operator back to a usable log view,
                // rather than leaving a half-entered filter that hides every
                // line.
                logs.search.clear();
                logs.filter();
                logs.search_editing = false;
                logs.scroll_to_bottom();
            }
            KeyCode::Enter => {
                logs.search_editing = false;
                logs.scroll_to_bottom();
            }
            KeyCode::Backspace => {
                logs.search.pop();
                logs.filter();
                logs.scroll_to_bottom();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                logs.search.push(c);
                logs.filter();
                logs.scroll_to_bottom();
            }
            _ => {}
        }
        return Ok(false);
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.mode = logs.close(),
        KeyCode::Char(':') => {
            app.mode = logs.close();
            app.focus = Focus::Command;
            app.command_input = ":".into();
            app.update_autocomplete();
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if logs.sidebar_visible {
                logs.files_focus = !logs.files_focus;
            }
        }
        KeyCode::Char('\\') => logs.toggle_sidebar(),
        KeyCode::Char('m') => logs.toggle_combined(),
        KeyCode::Char('v') if logs.combined => {
            logs.options_open = true;
            logs.options_ports = false;
            logs.option_selected = 0;
        }
        KeyCode::Char('t') if logs.combined => {
            logs.time_editing = true;
            logs.time_input.clear();
        }
        KeyCode::Char('+') if logs.combined => logs.jump_time(60),
        KeyCode::Char('-') if logs.combined => logs.jump_time(-60),
        KeyCode::Char('n') if logs.combined && !logs.files_focus => logs.jump_record(false, false),
        KeyCode::Char('N') if logs.combined && !logs.files_focus => logs.jump_record(true, false),
        KeyCode::Char('e') if logs.combined && !logs.files_focus => logs.jump_record(false, true),
        KeyCode::Char('E') if logs.combined && !logs.files_focus => logs.jump_record(true, true),
        KeyCode::Char('S') => logs.save_view(),
        KeyCode::Char('V') => {
            let text = logs.visible_text();
            let result = crate::external_editor::edit_text_in_external_editor(
                &text,
                Some(std::path::Path::new("logs.txt")),
                "log view",
            );
            app.needs_terminal_refresh = true;
            app.server_logs.message = match result {
                Ok(_) => "Returned from log view editor.".into(),
                Err(e) => format!("Editor: {e:#}"),
            };
        }
        KeyCode::Char('F') if logs.files_focus => {
            // The initial log view intentionally loads only cached/tracked
            // streams. Finding another current-day log should therefore make
            // the inventory available as part of the find action itself.
            logs.file_filter_editing = true;
            logs.refresh(&app.rt);
        }
        KeyCode::Char('/') => {
            logs.search_editing = true;
            logs.files_focus = false;
        }
        KeyCode::Char('c') if logs.files_focus => logs.clear_file_filter(),
        KeyCode::Char('c') => {
            logs.search.clear();
            logs.filter();
            logs.scroll_to_bottom();
        }
        KeyCode::Char('r') => logs.refresh(&app.rt),
        KeyCode::Char('s') => logs.sync(&app.rt, None),
        KeyCode::Char('o') => logs.toggle_subscription(&app.rt),
        KeyCode::Char('a') => logs.toggle_all_cached(),
        KeyCode::Char('p') if logs.combined && !logs.files_focus => {
            logs.options_open = true;
            logs.options_ports = true;
            logs.option_selected = 0;
        }
        KeyCode::Char('p') => logs.cycle_selected_port_filter(),
        KeyCode::Char('i') => logs.toggle_include_only_marked_ports(),
        KeyCode::Char('P') => logs.clear_port_filters(),
        KeyCode::Char('D') => logs.cycle_scope_days(&app.rt),
        KeyCode::Char('C') => logs.clear_cached_logs(&app.rt),
        KeyCode::Enter => {
            logs.download(&app.rt);
            logs.files_focus = false;
        }
        // `d` remains a convenient fetch key while choosing a file.  In the
        // content pane it is a pager key, matching the other full-screen
        // readers and making the tail-first view easy to navigate.
        KeyCode::Char('d') if logs.files_focus => logs.download(&app.rt),
        KeyCode::Char('b') => logs.download_full(&app.rt),
        KeyCode::Char('f') => {
            logs.following = !logs.following;
            if logs.following {
                logs.scroll_to_bottom();
                logs.sync(&app.rt, None);
            }
        }
        KeyCode::Char('x') => {
            logs.cancel();
            logs.following = false;
            logs.message = "Download cancelled; following paused.".into();
        }
        KeyCode::Char('j') | KeyCode::Down if logs.files_focus => logs.select_visible_next(),
        KeyCode::Char('k') | KeyCode::Up if logs.files_focus => logs.select_visible_previous(),
        KeyCode::Char('j') | KeyCode::Down => logs.scroll = logs.scroll.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => logs.scroll = logs.scroll.saturating_sub(1),
        KeyCode::Char('d') | KeyCode::PageDown | KeyCode::Char(' ') => {
            logs.scroll = logs.scroll.saturating_add(logs.view_height.max(1))
        }
        KeyCode::Char('u') | KeyCode::PageUp => {
            logs.scroll = logs.scroll.saturating_sub(logs.view_height.max(1))
        }
        KeyCode::Char('g') | KeyCode::Home if logs.files_focus => logs.select_visible_first(),
        KeyCode::Char('G') | KeyCode::End if logs.files_focus => logs.select_visible_last(),
        KeyCode::Char('g') | KeyCode::Home => logs.scroll = 0,
        KeyCode::Char('G') | KeyCode::End => logs.scroll = logs.matches.len().saturating_sub(1),
        KeyCode::Char('h') | KeyCode::Left => logs.horizontal = logs.horizontal.saturating_sub(8),
        KeyCode::Char('l') | KeyCode::Right => logs.horizontal = logs.horizontal.saturating_add(8),
        _ => {}
    }
    Ok(false)
}
