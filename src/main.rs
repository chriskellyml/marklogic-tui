mod client;
mod config;
mod events;
mod external_editor;
mod query_file;
mod query_result_cache;
mod tracked_folder;
mod ui;

use anyhow::Result;
use chrono::{DateTime, Local};
use clap::Parser;
use client::{AppServerEndpoint, AuthType, MarkLogicClient, SearchResult, ServerConfig};
use config::AppConfig;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use edtui::{
    EditorEventHandler, EditorState, EditorTheme, EditorView, LineNumbers, Lines, SyntaxHighlighter,
};
#[cfg(test)]
use events::{
    handle_command_key, handle_filter_key, handle_fullscreen_key, handle_log_viewer_key,
    handle_navigation_mode_key, handle_query_insert_transition_key, handle_query_key,
    handle_results_key, handle_return_to_start_page_key, handle_server_delete_confirm_key,
    handle_servers_interface_key, map_query_navigation_key,
};
#[cfg(test)]
use external_editor::temp_editor_path;
use flate2::{Compression, write::GzEncoder};
use query_file::{
    QueryExecutionKind, discover_query_files, display_query_path, editor_lines,
    is_supported_query_file, load_query_file, query_execution_kind, save_query_file,
    should_autosave,
};
use query_result_cache::{
    QueryResultSnapshot, load_query_result_snapshot, move_query_result_cache,
    remove_query_result_cache, save_query_result_snapshot,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Widget, Wrap,
    },
};
use ratatui_textarea::{Input, TextArea};
use std::str::FromStr;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tracing::{debug, error, info, warn};
use tracked_folder::{TrackedFolderEntry, TrackedFolderStore, canonicalize_folder};
use ui::{display_folder_path, format_document_detail, resolve_folder_input};

const LOG_FILE_PATH: &str = "/tmp/marklogic-tui.log";

#[derive(Parser, Debug)]
struct Args {
    /// Use an alternative inline editor. Supported value: "edtui" (Vim-inspired, with syntax highlighting).
    #[arg(long, value_name = "EDITOR")]
    inline_editor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Focus {
    Command,
    Query,
    Results,
    Filter,
}

#[derive(Debug, Clone, PartialEq)]
enum EditMode {
    Navigate,
    Insert,
}

#[derive(Debug, Clone, PartialEq)]
enum AppMode {
    StartPage,
    Normal,
    FullScreenView,
    LogViewer,
    Interface(AppInterface),
    ServerForm,
    ServerDeleteConfirm,
    AppServerList,
    AppServerForm,
    AppServerDeleteConfirm,
    CollectionSelect,
    DeleteConfirm,
    QueryFileSelect,
    QueryFileCreate,
    QueryFileRename,
    QueryFileDeleteConfirm,
    ModuleCloneSelect,
    TrackedFolderSelect,
    TrackedFolderAdd,
    TrackedFolderDeleteConfirm,
    TrackedFolderCacheClearConfirm,
    HelpOverlay,
    DocumentCreate,
    DocumentMetadataEdit,
}

#[derive(Debug, Clone, PartialEq)]
enum AppInterface {
    Servers,
}

#[derive(Debug, Clone, PartialEq)]
enum ServerFormMode {
    Add,
    Edit,
}

#[derive(Debug, Clone, PartialEq)]
enum ServersInterfaceFocus {
    Servers,
    Databases,
    AppServers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackedFolderMenuItem {
    Header(&'static str),
    Folder(TrackedFolderEntry),
}

struct App {
    config: AppConfig,
    client: Option<MarkLogicClient>,
    focus: Focus,
    edit_mode: EditMode,
    mode: AppMode,
    previous_mode: Option<AppMode>,
    modal_origin_focus: Option<Focus>,
    interface_origin_focus: Option<Focus>,
    command_input: String,
    query_editor: TextArea<'static>,
    query_visible: bool,
    needs_terminal_refresh: bool,
    should_quit: bool,
    query_root_dir: PathBuf,
    query_result_cache_dir: PathBuf,
    query_files: Vec<PathBuf>,
    active_query_file: Option<PathBuf>,
    query_file_list_state: ListState,
    module_clone_all_uris: Vec<String>,
    module_clone_filtered_uris: Vec<String>,
    module_clone_filter_input: String,
    module_clone_list_state: ListState,
    tracked_folders: TrackedFolderStore,
    tracked_folder_items: Vec<TrackedFolderMenuItem>,
    tracked_folder_list_state: ListState,
    tracked_folder_input: String,
    tracked_folder_delete_target: Option<PathBuf>,
    tracked_folder_cache_clear_target: Option<PathBuf>,
    file_list_visible: bool,
    file_delete_target: Option<PathBuf>,
    rename_file_input: String,
    rename_file_target: Option<PathBuf>,
    new_query_file_input: String,
    query_dirty: bool,
    query_last_edit: Option<Instant>,
    query_autosave_interval: Duration,
    results_text: String,
    status_message: String,
    transient_status_message: Option<String>,
    transient_status_expires_at: Option<Instant>,
    // Query results (individual parts from eval)
    query_results: Vec<String>,
    query_results_timestamp: Option<SystemTime>,
    query_results_state: TableState,
    // Listing state
    records: Vec<SearchResult>,
    list_state: TableState,
    selected_indices: Vec<usize>,
    current_page: usize,
    page_size: usize,
    total_results: Option<usize>,
    current_collection: Option<String>,
    uri_filter: Option<String>,
    filter_input: String,
    last_esc: Option<Instant>,
    last_query_results_g: Option<Instant>,
    // Full screen view
    active_document: Option<client::DocumentDetail>,
    full_view_content: String,
    full_view_scroll: u16,
    last_fullscreen_g: Option<Instant>,
    fullscreen_area_height: u16,
    log_file_path: PathBuf,
    log_view_content: String,
    log_view_scroll: u16,
    log_viewer_area_height: u16,
    last_log_viewer_g: Option<Instant>,
    last_logged_status_message: String,
    // Server management interface
    server_form_mode: ServerFormMode,
    server_form_step: usize,
    server_form_fields: Vec<String>,
    server_edit_target: Option<String>,
    server_delete_target: Option<String>,
    servers_interface_focus: ServersInterfaceFocus,
    // App server management
    app_server_form_mode: ServerFormMode,
    app_server_form_step: usize,
    app_server_form_fields: Vec<String>,
    app_server_edit_target: Option<String>,
    app_server_delete_target: Option<String>,
    app_server_list_server_name: Option<String>,
    // Document form (create / metadata edit)
    doc_form_step: usize,
    doc_form_uri: String,
    doc_form_collections: String,
    doc_form_quality: String,
    doc_form_content: String,
    // Autocomplete
    autocomplete_suggestions: Vec<&'static str>,
    autocomplete_selected: usize,
    // Database selection
    database_list: Vec<String>,
    database_list_state: ListState,
    app_server_list: Vec<AppServerEndpoint>,
    app_server_list_state: ListState,
    // Server selection
    server_list: Vec<String>,
    server_list_state: ListState,
    // Collection selection
    collection_list: Vec<String>,
    collection_list_state: ListState,
    // Runtime for async
    rt: Runtime,
    // Edtui alternative editor
    use_edtui: bool,
    edtui_state: EditorState,
    edtui_handler: EditorEventHandler,
}

impl App {
    fn build_client_from_config(config: &AppConfig) -> Option<MarkLogicClient> {
        let server = config.effective_active_server_config()?;
        let mut client = MarkLogicClient::new(server).ok()?;
        if let Some(db) = &config.active_database {
            client.set_database(db.clone());
        }
        if let Some(modules_db) = &config.active_modules_database {
            client.set_modules_database(modules_db.clone());
        }
        Some(client)
    }

    fn new(use_edtui: bool) -> Result<Self> {
        let config = AppConfig::load()?;
        let rt = Runtime::new()?;
        let mut tracked_folders = TrackedFolderStore::load()?;
        let query_root_dir =
            canonicalize_folder(&env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))?;
        tracked_folders.record_access(&query_root_dir)?;
        tracked_folders.save()?;
        let query_result_cache_dir = query_root_dir.join(".marklogic-tui");
        let query_files = discover_query_files(&query_root_dir)?;
        let active_query_file = query_files.first().cloned();
        let client = Self::build_client_from_config(&config);

        let mut app_result = Ok(Self {
            config,
            client,
            focus: Focus::Command,
            edit_mode: EditMode::Navigate,
            mode: AppMode::StartPage,
            previous_mode: None,
            modal_origin_focus: None,
            interface_origin_focus: None,
            command_input: String::new(),
            query_editor: Self::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            should_quit: false,
            query_root_dir,
            query_result_cache_dir,
            query_files,
            active_query_file,
            query_file_list_state: ListState::default(),
            module_clone_all_uris: Vec::new(),
            module_clone_filtered_uris: Vec::new(),
            module_clone_filter_input: String::new(),
            module_clone_list_state: ListState::default(),
            tracked_folders,
            tracked_folder_items: Vec::new(),
            tracked_folder_list_state: ListState::default(),
            tracked_folder_input: String::new(),
            tracked_folder_delete_target: None,
            tracked_folder_cache_clear_target: None,
            file_list_visible: true,
            file_delete_target: None,
            rename_file_input: String::new(),
            rename_file_target: None,
            new_query_file_input: String::new(),
            query_dirty: false,
            query_last_edit: None,
            query_autosave_interval: Duration::from_secs(2),
            results_text: String::new(),
            status_message: String::new(),
            transient_status_message: None,
            transient_status_expires_at: None,
            query_results: Vec::new(),
            query_results_timestamp: None,
            query_results_state: TableState::default(),
            records: Vec::new(),
            list_state: TableState::default(),
            selected_indices: Vec::new(),
            current_page: 0,
            page_size: 20,
            total_results: None,
            current_collection: None,
            uri_filter: None,
            filter_input: String::new(),
            last_esc: None,
            last_query_results_g: None,
            active_document: None,
            full_view_content: String::new(),
            full_view_scroll: 0,
            last_fullscreen_g: None,
            fullscreen_area_height: 0,
            log_file_path: PathBuf::from(LOG_FILE_PATH),
            log_view_content: String::new(),
            log_view_scroll: 0,
            log_viewer_area_height: 0,
            last_log_viewer_g: None,
            last_logged_status_message: String::new(),
            server_form_mode: ServerFormMode::Add,
            server_form_step: 0,
            server_form_fields: vec![String::new(); 6], // name, host, user, pass, auth, insecure
            server_edit_target: None,
            server_delete_target: None,
            servers_interface_focus: ServersInterfaceFocus::Servers,
            app_server_form_mode: ServerFormMode::Add,
            app_server_form_step: 0,
            app_server_form_fields: vec![String::new(); 5], // name, port, secure, content_db, modules_db
            app_server_edit_target: None,
            app_server_delete_target: None,
            app_server_list_server_name: None,
            doc_form_step: 0,
            doc_form_uri: String::new(),
            doc_form_collections: String::new(),
            doc_form_quality: "0".to_string(),
            doc_form_content: String::new(),
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
            database_list: Vec::new(),
            database_list_state: ListState::default(),
            app_server_list: Vec::new(),
            app_server_list_state: ListState::default(),
            server_list: Vec::new(),
            server_list_state: ListState::default(),
            collection_list: Vec::new(),
            collection_list_state: ListState::default(),
            rt,
            use_edtui,
            edtui_state: EditorState::default(),
            edtui_handler: EditorEventHandler::default(),
        });

        if let Ok(ref mut app) = app_result {
            app.initialize_query_editor();
            app.rebuild_tracked_folder_items();
        }

        // Calculate initial page_size from terminal size
        if let Ok(ref mut app) = app_result {
            app.recalculate_page_size();
        }

        // Start in the centered command-first home screen.
        if let Ok(ref mut app) = app_result {
            app.enter_start_page();
        }

        app_result
    }

    const COMMANDS: &[(&str, &str)] = &[
        // Server management
        (
            ":servers",
            "Open the server management interface [a=add, e=edit, d=remove]",
        ),
        (":server-add", "Open server management in add mode"),
        (":databases", "List databases"),
        // Document listing
        (":list", "List all documents (paged)"),
        (":collections", "Show collections"),
        (":list:<collection>", "List documents in collection"),
        (":clear", "Clear collection filter and reset page"),
        // Query management
        (":query", "Show the active query file"),
        (":query-files", "Open the query file picker"),
        (":query-open", "Open the query file picker"),
        (":folders", "Open the tracked folder selector"),
        // Other
        (":logs", "Open the application log viewer"),
        (":tdes", "List Template Driven Extraction templates"),
        (":quit", "Quit the application"),
    ];

    fn new_query_editor(lines: Vec<String>) -> TextArea<'static> {
        let mut ta = TextArea::new(if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        });
        ta.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title("Query [Alt+Enter/F5 run, c clone, e edit, Ctrl+S save, Ctrl+O switch]"),
        );
        ta
    }

    fn query_text(&self) -> String {
        if self.use_edtui {
            // Lines::iter() yields (Option<&char>, Index2) for each position.
            // Group by row index to reconstruct the text.
            let mut rows: Vec<String> = Vec::new();
            let mut cur_row = usize::MAX;
            for (opt_char, idx) in self.edtui_state.lines.iter() {
                if idx.row != cur_row {
                    rows.push(String::new());
                    cur_row = idx.row;
                }
                if let Some(c) = opt_char {
                    if let Some(last) = rows.last_mut() {
                        last.push(*c);
                    }
                }
            }
            if rows.is_empty() {
                rows.push(String::new());
            }
            rows.join("\n")
        } else {
            self.query_editor.lines().join("\n")
        }
    }

    fn initialize_query_editor(&mut self) {
        if let Some(path) = self.active_query_file.clone() {
            match self.load_query_file(path) {
                Ok(()) => {}
                Err(e) => {
                    self.query_editor = Self::new_query_editor(Vec::new());
                    if self.use_edtui {
                        self.edtui_state = EditorState::default();
                    }
                    self.active_query_file = None;
                    self.status_message = e.to_string();
                }
            }
        } else {
            self.query_editor = Self::new_query_editor(Vec::new());
            self.status_message = format!(
                "No supported query files found in {}",
                self.query_root_dir.display()
            );
        }
    }

    fn update_autocomplete(&mut self) {
        let normalized_input = if self.command_input.is_empty() {
            String::new()
        } else if self.command_input.starts_with(':') {
            self.command_input.clone()
        } else {
            format!(":{}", self.command_input)
        };

        if normalized_input.len() > 1 {
            self.autocomplete_suggestions = Self::COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(&normalized_input))
                .map(|(cmd, _)| *cmd)
                .collect();
        } else if normalized_input == ":" {
            self.autocomplete_suggestions = Self::COMMANDS.iter().map(|(cmd, _)| *cmd).collect();
        } else {
            self.autocomplete_suggestions.clear();
        }
        self.autocomplete_selected = 0;
    }

    fn refresh_query_files(&mut self) -> Result<()> {
        self.query_files = discover_query_files(&self.query_root_dir)?;
        if self.query_files.is_empty() {
            self.query_file_list_state.select(None);
        } else {
            let selected = self
                .active_query_file
                .as_ref()
                .and_then(|active| self.query_files.iter().position(|path| path == active))
                .unwrap_or(0);
            self.query_file_list_state.select(Some(selected));
        }
        Ok(())
    }

    fn rebuild_tracked_folder_items(&mut self) {
        let preferred_selection = self
            .selected_tracked_folder_path()
            .unwrap_or_else(|| self.query_root_dir.clone());
        let mut items = Vec::new();

        let favorites = self.tracked_folders.favorite_entries();
        if !favorites.is_empty() {
            items.push(TrackedFolderMenuItem::Header("Favorites"));
            items.extend(favorites.into_iter().map(TrackedFolderMenuItem::Folder));
        }

        let recents = self.tracked_folders.recent_entries();
        if !recents.is_empty() {
            items.push(TrackedFolderMenuItem::Header("Recent"));
            items.extend(recents.into_iter().map(TrackedFolderMenuItem::Folder));
        }

        self.tracked_folder_items = items;
        let selected = self
            .tracked_folder_item_index(&preferred_selection)
            .or_else(|| self.first_tracked_folder_item_index());
        self.tracked_folder_list_state.select(selected);
    }

    fn first_tracked_folder_item_index(&self) -> Option<usize> {
        self.tracked_folder_items
            .iter()
            .position(|item| matches!(item, TrackedFolderMenuItem::Folder(_)))
    }

    fn last_tracked_folder_item_index(&self) -> Option<usize> {
        self.tracked_folder_items
            .iter()
            .rposition(|item| matches!(item, TrackedFolderMenuItem::Folder(_)))
    }

    fn tracked_folder_item_index(&self, path: &Path) -> Option<usize> {
        let path_string = path.to_string_lossy().to_string();
        self.tracked_folder_items.iter().position(|item| {
            matches!(item, TrackedFolderMenuItem::Folder(entry) if entry.path == path_string)
        })
    }

    fn selected_tracked_folder_path(&self) -> Option<PathBuf> {
        let selected = self.tracked_folder_list_state.selected()?;
        match self.tracked_folder_items.get(selected)? {
            TrackedFolderMenuItem::Folder(entry) => Some(PathBuf::from(&entry.path)),
            TrackedFolderMenuItem::Header(_) => None,
        }
    }

    fn move_tracked_folder_selection(&mut self, delta: isize) {
        if self.tracked_folder_items.is_empty() {
            self.tracked_folder_list_state.select(None);
            return;
        }

        let len = self.tracked_folder_items.len() as isize;
        let start = self
            .tracked_folder_list_state
            .selected()
            .map(|idx| idx as isize);
        let mut index = start.unwrap_or(if delta >= 0 { -1 } else { len });

        loop {
            index += delta;
            if index < 0 || index >= len {
                break;
            }
            if matches!(
                self.tracked_folder_items.get(index as usize),
                Some(TrackedFolderMenuItem::Folder(_))
            ) {
                self.tracked_folder_list_state.select(Some(index as usize));
                return;
            }
        }
    }

    fn open_tracked_folder_picker(&mut self) {
        self.rebuild_tracked_folder_items();
        if let Some(index) = self.tracked_folder_item_index(&self.query_root_dir) {
            self.tracked_folder_list_state.select(Some(index));
        }
        self.open_modal(AppMode::TrackedFolderSelect);
    }

    fn open_tracked_folder_add(&mut self) {
        self.tracked_folder_input.clear();
        self.open_modal(AppMode::TrackedFolderAdd);
    }

    fn add_tracked_folder_from_input(&mut self) {
        let input = self.tracked_folder_input.trim().to_string();
        if input.is_empty() {
            self.status_message = "Enter a folder path.".to_string();
            return;
        }

        let candidate = resolve_folder_input(&self.query_root_dir, &input);
        match self.tracked_folders.add_folder(&candidate) {
            Ok(path) => {
                if let Err(e) = self.switch_query_root(path) {
                    self.status_message = format!("Failed to switch folder: {}", e);
                    self.mode = AppMode::TrackedFolderAdd;
                }
            }
            Err(e) => {
                self.status_message = format!("Failed to add folder: {}", e);
            }
        }
    }

    fn switch_query_root(&mut self, path: PathBuf) -> Result<()> {
        let canonical_root = canonicalize_folder(&path)?;

        if canonical_root == self.query_root_dir {
            self.tracked_folders.record_access(&canonical_root)?;
            self.tracked_folders.save()?;
            self.rebuild_tracked_folder_items();
            self.close_modal_with_focus(Focus::Query);
            self.status_message = format!(
                "Folder already active: {}",
                display_folder_path(&canonical_root)
            );
            return Ok(());
        }

        self.save_query_file_if_dirty()?;

        self.query_root_dir = canonical_root.clone();
        self.query_result_cache_dir = canonical_root.join(".marklogic-tui");
        self.active_query_file = None;
        self.query_dirty = false;
        self.query_last_edit = None;

        self.tracked_folders.record_access(&canonical_root)?;
        self.tracked_folders.save()?;

        self.refresh_query_files()?;

        if let Some(path) = self.query_files.first().cloned() {
            self.load_query_file(path)?;
            self.status_message = format!(
                "Switched query folder: {}",
                display_folder_path(&canonical_root)
            );
        } else {
            self.query_editor = Self::new_query_editor(Vec::new());
            self.active_query_file = None;
            self.clear_query_results_view();
            self.focus_query_panel();
            self.status_message = format!(
                "Switched query folder: {} (no supported query files found)",
                display_folder_path(&canonical_root)
            );
        }

        self.rebuild_tracked_folder_items();
        self.close_modal_with_focus(Focus::Query);
        Ok(())
    }

    fn switch_to_selected_tracked_folder(&mut self) {
        let Some(path) = self.selected_tracked_folder_path() else {
            self.status_message = "No folder selected.".to_string();
            return;
        };

        if let Err(e) = self.switch_query_root(path) {
            self.status_message = format!("Failed to switch folder: {}", e);
            self.mode = AppMode::TrackedFolderSelect;
        }
    }

    fn toggle_selected_tracked_folder_favorite(&mut self) {
        let Some(path) = self.selected_tracked_folder_path() else {
            self.status_message = "No folder selected.".to_string();
            return;
        };

        let Some(is_favorite) = self.tracked_folders.toggle_favorite(&path) else {
            self.status_message = "No folder selected.".to_string();
            return;
        };

        match self.tracked_folders.save() {
            Ok(()) => {
                self.rebuild_tracked_folder_items();
                self.status_message = if is_favorite {
                    format!("Added favorite folder: {}", display_folder_path(&path))
                } else {
                    format!("Removed favorite folder: {}", display_folder_path(&path))
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to update favorite folder: {}", e);
            }
        }
    }

    fn start_delete_selected_tracked_folder(&mut self) {
        self.tracked_folder_delete_target = self.selected_tracked_folder_path();
        if self.tracked_folder_delete_target.is_some() {
            self.mode = AppMode::TrackedFolderDeleteConfirm;
        } else {
            self.status_message = "No folder selected.".to_string();
        }
    }

    fn confirm_delete_selected_tracked_folder(&mut self) {
        let Some(path) = self.tracked_folder_delete_target.take() else {
            self.mode = AppMode::TrackedFolderSelect;
            return;
        };

        if !self.tracked_folders.remove_folder(&path) {
            self.status_message =
                format!("Tracked folder not found: {}", display_folder_path(&path));
            self.mode = AppMode::TrackedFolderSelect;
            return;
        }

        match self.tracked_folders.save() {
            Ok(()) => {
                self.rebuild_tracked_folder_items();
                self.status_message = if path == self.query_root_dir {
                    format!(
                        "Removed tracked folder: {} (current folder stays active)",
                        display_folder_path(&path)
                    )
                } else {
                    format!("Removed tracked folder: {}", display_folder_path(&path))
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to remove tracked folder: {}", e);
            }
        }

        self.mode = AppMode::TrackedFolderSelect;
    }

    fn start_clear_selected_tracked_folder_cache(&mut self) {
        self.tracked_folder_cache_clear_target = self.selected_tracked_folder_path();
        if self.tracked_folder_cache_clear_target.is_some() {
            self.mode = AppMode::TrackedFolderCacheClearConfirm;
        } else {
            self.status_message = "No folder selected.".to_string();
        }
    }

    fn confirm_clear_selected_tracked_folder_cache(&mut self) {
        let Some(path) = self.tracked_folder_cache_clear_target.take() else {
            self.mode = AppMode::TrackedFolderSelect;
            return;
        };

        let cache_dir = path.join(".marklogic-tui");
        if !cache_dir.exists() {
            self.status_message =
                format!("No cached results found for {}", display_folder_path(&path));
            self.mode = AppMode::TrackedFolderSelect;
            return;
        }

        match fs::remove_dir_all(&cache_dir) {
            Ok(()) => {
                self.status_message =
                    format!("Cleared cached results for {}", display_folder_path(&path));
            }
            Err(e) => {
                self.status_message = format!("Failed to clear cached results: {}", e);
            }
        }

        self.mode = AppMode::TrackedFolderSelect;
    }

    fn active_query_file_label(&self) -> String {
        self.active_query_file
            .as_ref()
            .map(|path| display_query_path(path, &self.query_root_dir))
            .unwrap_or_else(|| "(no query file)".to_string())
    }

    fn query_title(&self) -> String {
        let dirty = if self.query_dirty { "*" } else { "" };
        format!("Query: {}{}", self.active_query_file_label(), dirty)
    }

    fn edit_mode_label(&self) -> &'static str {
        match self.edit_mode {
            EditMode::Navigate => "NORMAL",
            EditMode::Insert => "INSERT",
        }
    }

    fn status_mode_label(&self) -> &'static str {
        match (&self.mode, &self.server_form_mode) {
            (AppMode::Interface(AppInterface::Servers), _) => "SERVERS",
            (AppMode::ServerForm, ServerFormMode::Add) => "SERVER ADD",
            (AppMode::ServerForm, ServerFormMode::Edit) => "SERVER EDIT",
            (AppMode::ServerDeleteConfirm, _) => "SERVER DELETE",
            (AppMode::LogViewer, _) => "LOG VIEW",
            _ => self.edit_mode_label(),
        }
    }

    fn edit_mode_style(&self) -> Style {
        let text = Color::Rgb(6, 12, 28);
        match self.edit_mode {
            EditMode::Navigate => Style::default().fg(text).bg(Color::White).add_modifier(Modifier::BOLD),
            EditMode::Insert => Style::default().fg(text).bg(Color::Red).add_modifier(Modifier::BOLD),
        }
    }

    fn edit_mode_accent_color(&self) -> Color {
        match self.edit_mode {
            EditMode::Navigate => Color::Cyan,
            EditMode::Insert => Color::Red,
        }
    }

    fn status_identity_spans(&self) -> Vec<Span<'static>> {
        let server = self
            .config
            .active_server
            .as_deref()
            .unwrap_or("(no server)");
        let port = self
            .config
            .effective_active_server_config()
            .map(|s| s.port.to_string())
            .unwrap_or_else(|| "-".to_string());
        let db = self
            .config
            .active_database
            .as_deref()
            .unwrap_or("(no database)");

        let text = Color::Rgb(6, 12, 28);
        let separator = Style::default().fg(Color::Rgb(100, 100, 100));
        let server_style = Style::default().fg(text).add_modifier(Modifier::BOLD);
        let port_style = Style::default().fg(text).add_modifier(Modifier::BOLD);
        let database_style = Style::default().fg(text).add_modifier(Modifier::BOLD);

        vec![
            Span::styled(" · ", separator),
            Span::styled(server.to_string(), server_style),
            Span::styled(":", separator),
            Span::styled(port, port_style),
            Span::styled(" · ", separator),
            Span::styled(db.to_string(), database_style),
        ]
    }

    fn focus_command_panel(&mut self) {
        self.focus = Focus::Command;
        self.edit_mode = EditMode::Navigate;
    }

    fn apply_focus(&mut self, focus: Focus) {
        match focus {
            Focus::Command => self.focus_command_panel(),
            Focus::Query => self.focus_query_panel(),
            Focus::Results => self.focus_results_panel(),
            Focus::Filter => {
                self.focus = Focus::Filter;
                self.edit_mode = EditMode::Navigate;
            }
        }
    }

    fn is_modal_mode(mode: &AppMode) -> bool {
        matches!(
            mode,
            AppMode::ServerForm
                | AppMode::ServerDeleteConfirm
                | AppMode::CollectionSelect
                | AppMode::DeleteConfirm
                | AppMode::QueryFileSelect
                | AppMode::QueryFileCreate
                | AppMode::QueryFileRename
                | AppMode::QueryFileDeleteConfirm
                | AppMode::ModuleCloneSelect
                | AppMode::TrackedFolderSelect
                | AppMode::TrackedFolderAdd
                | AppMode::TrackedFolderDeleteConfirm
                | AppMode::TrackedFolderCacheClearConfirm
        )
    }

    fn is_interface_mode(mode: &AppMode) -> bool {
        matches!(mode, AppMode::Interface(_))
    }

    fn open_interface(&mut self, interface: AppInterface) {
        if !Self::is_interface_mode(&self.mode) {
            self.interface_origin_focus = Some(self.focus.clone());
        }
        self.modal_origin_focus = None;
        self.mode = AppMode::Interface(interface);
        self.edit_mode = EditMode::Navigate;
    }

    fn close_interface_restore_focus(&mut self) {
        let target_focus = self
            .interface_origin_focus
            .take()
            .unwrap_or_else(|| self.focus.clone());
        self.mode = AppMode::Normal;
        self.apply_focus(target_focus);
    }

    fn open_modal(&mut self, mode: AppMode) {
        if !Self::is_modal_mode(&mode) {
            self.mode = mode;
            return;
        }

        if !Self::is_modal_mode(&self.mode) {
            self.modal_origin_focus = Some(self.focus.clone());
        }

        self.mode = mode;
    }

    fn close_modal_restore_focus(&mut self) {
        let target_focus = self
            .modal_origin_focus
            .take()
            .unwrap_or_else(|| self.focus.clone());
        self.mode = AppMode::Normal;
        self.apply_focus(target_focus);
    }

    fn close_modal_with_focus(&mut self, focus: Focus) {
        self.modal_origin_focus = None;
        self.mode = AppMode::Normal;
        self.apply_focus(focus);
    }

    fn enter_start_page(&mut self) {
        self.mode = AppMode::StartPage;
        self.modal_origin_focus = None;
        self.interface_origin_focus = None;
        self.focus_command_panel();
        self.query_visible = false;
        self.autocomplete_suggestions.clear();
        self.autocomplete_selected = 0;
        self.command_input.clear();
    }

    fn focus_query_panel(&mut self) {
        self.query_visible = true;
        self.focus = Focus::Query;
        self.edit_mode = EditMode::Navigate;
        if self.query_file_list_state.selected().is_none() && !self.query_files.is_empty() {
            let selected = self
                .active_query_file
                .as_ref()
                .and_then(|active| self.query_files.iter().position(|path| path == active))
                .unwrap_or(0);
            self.query_file_list_state.select(Some(selected));
        }
    }

    fn focus_results_panel(&mut self) {
        self.focus = Focus::Results;
        self.edit_mode = EditMode::Navigate;
    }

    fn toggle_file_list(&mut self) {
        self.file_list_visible = !self.file_list_visible;
    }

    fn cycle_panel_focus(&mut self) {
        match self.focus {
            Focus::Command | Focus::Filter => self.focus_query_panel(),
            Focus::Query => self.focus_results_panel(),
            Focus::Results => self.focus_query_panel(),
        }
    }

    fn cycle_panel_focus_backward(&mut self) {
        match self.focus {
            Focus::Command | Focus::Filter => self.focus_results_panel(),
            Focus::Query => self.focus_results_panel(),
            Focus::Results => self.focus_query_panel(),
        }
    }

    fn open_filter_input(&mut self) {
        self.focus = Focus::Filter;
        self.filter_input = self.uri_filter.clone().unwrap_or_default();
    }

    fn enter_insert_mode(&mut self) {
        if self.focus == Focus::Query {
            self.edit_mode = EditMode::Insert;
        } else {
            self.status_message = "Insert mode only applies to the query editor.".to_string();
        }
    }

    fn exit_insert_mode(&mut self) {
        self.edit_mode = EditMode::Navigate;
        self.last_esc = None;
    }

    fn replace_query_contents(&mut self, contents: &str) {
        self.query_editor = Self::new_query_editor(editor_lines(contents));
        if self.use_edtui {
            self.edtui_state = EditorState::new(Lines::from(contents));
        }
        self.focus_query_panel();
    }

    fn open_query_in_external_editor(&mut self) {
        debug!(
            active_query_file = ?self.active_query_file,
            "opening query in external editor"
        );
        let original_contents = self.query_text();
        let edit_result = external_editor::edit_text_in_external_editor(
            &original_contents,
            self.active_query_file.as_deref(),
            "query",
        );
        self.needs_terminal_refresh = true;

        match edit_result {
            Ok(edited_contents) => {
                self.replace_query_contents(&edited_contents);
                match self.active_query_file.as_ref() {
                    Some(path) => match load_query_file(path) {
                        Ok(saved_contents) => {
                            self.query_dirty = edited_contents != saved_contents;
                            self.query_last_edit = self.query_dirty.then_some(Instant::now());
                            self.status_message = format!(
                                "Loaded external edits for {}",
                                display_query_path(path, &self.query_root_dir)
                            );
                        }
                        Err(e) => {
                            self.query_dirty = true;
                            self.query_last_edit = Some(Instant::now());
                            self.status_message = format!(
                                "Loaded external edits, but couldn't compare {}: {}",
                                display_query_path(path, &self.query_root_dir),
                                e
                            );
                        }
                    },
                    None => {
                        self.query_dirty = false;
                        self.query_last_edit = None;
                        self.status_message =
                            "Loaded external edits into the query pane.".to_string();
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "external query edit failed");
                self.status_message = format!("External edit failed: {}", e);
            }
        }
    }

    fn refresh_log_viewer_content(&mut self) {
        match fs::read_to_string(&self.log_file_path) {
            Ok(content) => {
                self.log_view_content = content;
            }
            Err(e) => {
                self.log_view_content = format!(
                    "Failed to read log file {}: {}",
                    self.log_file_path.display(),
                    e
                );
            }
        }
    }

    fn log_viewer_max_scroll(&self) -> u16 {
        let total_lines = self.log_view_content.lines().count() as u16;
        let visible_lines = self.log_viewer_area_height.saturating_sub(2);
        total_lines.saturating_sub(visible_lines)
    }

    fn open_log_viewer(&mut self) {
        if self.mode != AppMode::LogViewer {
            self.previous_mode = Some(self.mode.clone());
        }
        self.mode = AppMode::LogViewer;
        self.last_log_viewer_g = None;
        self.refresh_log_viewer_content();
        self.log_view_scroll = self.log_viewer_max_scroll();
        debug!("opened log viewer");
    }

    fn close_log_viewer(&mut self) {
        self.mode = self.previous_mode.take().unwrap_or(AppMode::Normal);
        self.last_log_viewer_g = None;
    }

    fn open_log_in_external_editor(&mut self) {
        debug!(path = %self.log_file_path.display(), "opening log file in external editor");
        let edit_result = external_editor::open_file_in_external_editor(&self.log_file_path);
        self.needs_terminal_refresh = true;

        match edit_result {
            Ok(()) => {
                self.refresh_log_viewer_content();
                self.log_view_scroll = self.log_viewer_max_scroll();
                self.status_message = format!("Edited log: {}", self.log_file_path.display());
            }
            Err(e) => {
                error!(error = %e, "external log edit failed");
                self.status_message = format!("External edit failed: {}", e);
            }
        }
    }

    fn current_query_extension(&self) -> String {
        self.active_query_file
            .as_ref()
            .and_then(|path| path.extension())
            .and_then(|ext| ext.to_str())
            .filter(|ext| !ext.is_empty())
            .unwrap_or("xqy")
            .to_string()
    }

    fn default_new_query_file_name(&self) -> String {
        let extension = self.current_query_extension();
        let mut index = 1usize;
        loop {
            let file_name = format!("query-{}.{}", index, extension);
            let path = self.query_root_dir.join(&file_name);
            if !path.exists() && !self.query_files.iter().any(|existing| existing == &path) {
                return file_name;
            }
            index += 1;
        }
    }

    fn open_query_file_create(&mut self) {
        self.new_query_file_input = self.default_new_query_file_name();
        self.open_modal(AppMode::QueryFileCreate);
    }

    fn create_new_query_file_from_input(&mut self) {
        let file_name = self.new_query_file_input.trim().to_string();
        if file_name.is_empty() {
            self.status_message = "Enter a file name.".to_string();
            return;
        }

        let file_path = Path::new(&file_name);
        if file_path.components().count() != 1 {
            self.status_message =
                "Enter a file name only, not a nested or absolute path.".to_string();
            return;
        }

        let path = self.query_root_dir.join(file_path);
        if !is_supported_query_file(&path) {
            self.status_message =
                "Unsupported query file extension. Use js, mjs, sparql, sql, sjs, or xqy."
                    .to_string();
            return;
        }
        if path.exists() {
            self.status_message = format!(
                "Query file already exists: {}",
                display_query_path(&path, &self.query_root_dir)
            );
            return;
        }

        match save_query_file(&path, "") {
            Ok(()) => {
                if let Err(e) = self.refresh_query_files() {
                    self.status_message =
                        format!("Created file but failed to refresh picker: {}", e);
                    return;
                }
                if let Err(e) = self.load_query_file(path.clone()) {
                    self.status_message = format!("Created file but failed to open it: {}", e);
                    return;
                }
                self.close_modal_with_focus(Focus::Query);
                self.status_message = format!(
                    "Created query file: {}",
                    display_query_path(&path, &self.query_root_dir)
                );
            }
            Err(e) => {
                self.status_message = format!("Failed to create query file: {}", e);
            }
        }
    }

    fn delete_query_file(&mut self, path: PathBuf) {
        if let Err(e) = fs::remove_file(&path) {
            self.status_message = format!("Failed to delete file: {}", e);
            return;
        }

        let cache_warning =
            remove_query_result_cache(&self.query_result_cache_dir, &self.query_root_dir, &path)
                .err()
                .map(|e| e.to_string());

        self.status_message = format!(
            "Deleted query file: {}",
            display_query_path(&path, &self.query_root_dir)
        );
        if let Some(warning) = cache_warning {
            self.status_message
                .push_str(&format!(" (cache cleanup warning: {})", warning));
        }
        if self.active_query_file.as_ref() == Some(&path) {
            self.active_query_file = None;
            self.query_editor = Self::new_query_editor(Vec::new());
            self.query_dirty = false;
            self.query_last_edit = None;
        }
        if let Err(e) = self.refresh_query_files() {
            self.status_message = format!("Deleted file but failed to refresh list: {}", e);
        }
    }

    fn start_rename_query_file(&mut self) {
        if let Some(sel) = self.query_file_list_state.selected() {
            if let Some(path) = self.query_files.get(sel).cloned() {
                let name = display_query_path(&path, &self.query_root_dir);
                self.rename_file_input = name;
                self.rename_file_target = Some(path);
                self.open_modal(AppMode::QueryFileRename);
            }
        }
    }

    fn rename_query_file(&mut self) {
        let Some(old_path) = self.rename_file_target.take() else {
            self.close_modal_with_focus(Focus::Query);
            return;
        };
        let new_name = self.rename_file_input.trim().to_string();
        if new_name.is_empty() {
            self.status_message = "Enter a file name.".to_string();
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        let file_path = Path::new(&new_name);
        if file_path.components().count() != 1 {
            self.status_message =
                "Enter a file name only, not a nested or absolute path.".to_string();
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        let new_path = self.query_root_dir.join(file_path);
        if new_path.exists() {
            self.status_message = format!(
                "File already exists: {}",
                display_query_path(&new_path, &self.query_root_dir)
            );
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        if let Err(e) = fs::rename(&old_path, &new_path) {
            self.status_message = format!("Failed to rename file: {}", e);
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        if self.active_query_file.as_ref() == Some(&old_path) {
            self.active_query_file = Some(new_path.clone());
        }

        let cache_move_warning = move_query_result_cache(
            &self.query_result_cache_dir,
            &self.query_root_dir,
            &old_path,
            &new_path,
        )
        .err()
        .map(|e| e.to_string());

        if let Err(e) = self.refresh_query_files() {
            self.status_message = format!("Renamed file but failed to refresh list: {}", e);
        } else {
            self.status_message = format!(
                "Renamed to: {}",
                display_query_path(&new_path, &self.query_root_dir)
            );
            if let Some(warning) = cache_move_warning {
                self.status_message
                    .push_str(&format!(" (cache move warning: {})", warning));
            }
        }
        self.close_modal_with_focus(Focus::Query);
    }

    fn set_query_results(&mut self, results: Vec<String>) {
        self.records.clear();
        self.list_state.select(None);
        self.selected_indices.clear();
        self.total_results = None;
        self.query_results = results;
        self.query_results_timestamp = Some(SystemTime::now());
        self.query_results_state
            .select(if self.query_results.is_empty() {
                None
            } else {
                Some(0)
            });
        self.last_query_results_g = None;
        self.results_text.clear();
    }

    fn clear_query_results_view(&mut self) {
        self.records.clear();
        self.list_state.select(None);
        self.selected_indices.clear();
        self.total_results = None;
        self.query_results.clear();
        self.query_results_timestamp = None;
        self.query_results_state.select(None);
        self.last_query_results_g = None;
        self.results_text.clear();
    }

    fn persist_query_results_for_path(&mut self, path: &Path) {
        let snapshot = QueryResultSnapshot {
            query_results: self.query_results.clone(),
            selected_index: self.query_results_state.selected(),
            created_at: self
                .query_results_timestamp
                .map(|ts| ts.duration_since(UNIX_EPOCH).unwrap().as_secs()),
        };

        if let Err(e) = save_query_result_snapshot(
            &self.query_result_cache_dir,
            &self.query_root_dir,
            path,
            &snapshot,
        ) {
            self.status_message = format!("Query executed, but failed to cache results: {}", e);
        }
    }

    fn restore_query_results_for_path(&mut self, path: &Path) -> Result<Option<usize>> {
        let snapshot =
            load_query_result_snapshot(&self.query_result_cache_dir, &self.query_root_dir, path)?;

        if let Some(snapshot) = snapshot {
            self.records.clear();
            self.list_state.select(None);
            self.selected_indices.clear();
            self.total_results = None;
            self.query_results = snapshot.query_results;
            self.query_results_timestamp = snapshot
                .created_at
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
            let selected = snapshot
                .selected_index
                .filter(|idx| *idx < self.query_results.len())
                .or_else(|| {
                    if self.query_results.is_empty() {
                        None
                    } else {
                        Some(0)
                    }
                });
            self.query_results_state.select(selected);
            self.last_query_results_g = None;
            self.results_text.clear();
            Ok(Some(self.query_results.len()))
        } else {
            self.clear_query_results_view();
            Ok(None)
        }
    }

    fn set_active_document(&mut self, detail: client::DocumentDetail) {
        self.full_view_content = format_document_detail(&detail);
        self.active_document = Some(detail);
        self.full_view_scroll = 0;
        self.mode = AppMode::FullScreenView;
    }

    fn open_document_in_external_editor(&mut self) {
        debug!(
            has_active_document = self.active_document.is_some(),
            "opening document/content in external editor"
        );
        let (original_content, path, is_document) = if let Some(ref detail) = self.active_document {
            (
                detail.content.clone(),
                Some(Path::new(detail.uri.as_str())),
                true,
            )
        } else {
            (self.full_view_content.clone(), None, false)
        };

        let label = if is_document { "document" } else { "content" };
        let edit_result =
            external_editor::edit_text_in_external_editor(&original_content, path, label);
        self.needs_terminal_refresh = true;

        match edit_result {
            Ok(edited_contents) => {
                if edited_contents == original_content {
                    let msg = if is_document {
                        format!(
                            "No document changes to save for {}",
                            self.active_document.as_ref().unwrap().uri
                        )
                    } else {
                        "No changes to save.".to_string()
                    };
                    self.status_message = msg;
                    return;
                }

                if is_document {
                    let Some(client) = &self.client else {
                        self.status_message = "No server connected.".to_string();
                        return;
                    };

                    let client = client.clone();
                    let uri = self.active_document.as_ref().unwrap().uri.clone();
                    match self.rt.block_on(client.update_document(
                        &uri,
                        &edited_contents,
                        None,
                        None,
                    )) {
                        Ok(()) => {
                            let mut updated_detail = self.active_document.clone().unwrap();
                            updated_detail.content = edited_contents;
                            self.set_active_document(updated_detail.clone());
                            self.status_message = format!("Saved document: {}", updated_detail.uri);
                            if !self.records.is_empty() {
                                self.fetch_list();
                                self.mode = AppMode::FullScreenView;
                                self.active_document = Some(updated_detail);
                            }
                        }
                        Err(e) => {
                            self.status_message = format!("Document save failed: {}", e);
                        }
                    }
                } else {
                    self.full_view_content = edited_contents;
                    self.status_message = "Content updated.".to_string();
                }
            }
            Err(e) => {
                error!(error = %e, "external document/content edit failed");
                self.status_message = format!("External edit failed: {}", e);
            }
        }
    }

    fn open_document_create(&mut self) {
        self.doc_form_step = 0;
        self.doc_form_uri.clear();
        self.doc_form_collections.clear();
        self.doc_form_quality = "0".to_string();
        self.doc_form_content.clear();
        self.previous_mode = Some(self.mode.clone());
        self.mode = AppMode::DocumentCreate;
    }

    fn submit_document_create(&mut self) {
        let uri = self.doc_form_uri.trim().to_string();
        if uri.is_empty() {
            self.status_message = "URI is required.".to_string();
            return;
        }
        let collections: Vec<String> = self
            .doc_form_collections
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let quality = self.doc_form_quality.trim().parse::<i64>().unwrap_or(0);
        let content = self.doc_form_content.clone();
        let Some(client) = &self.client else {
            self.status_message = "No server connected.".to_string();
            return;
        };
        let client = client.clone();
        match self
            .rt
            .block_on(client.create_document(&uri, &content, &collections, quality))
        {
            Ok(()) => {
                self.status_message = format!("Created document: {}", uri);
                self.mode = self.previous_mode.clone().unwrap_or(AppMode::Normal);
                self.previous_mode = None;
                self.fetch_list();
            }
            Err(e) => {
                self.status_message = format!("Failed to create document: {}", e);
            }
        }
    }

    fn open_document_metadata_edit(&mut self) {
        if let Some(ref doc) = self.active_document {
            self.doc_form_step = 0;
            self.doc_form_uri = doc.uri.clone();
            self.doc_form_collections = doc.collections.join(", ");
            self.doc_form_quality = doc.quality.map(|q| q.to_string()).unwrap_or_default();
            self.doc_form_content = doc.content.clone();
            self.previous_mode = Some(self.mode.clone());
            self.mode = AppMode::DocumentMetadataEdit;
        }
    }

    fn submit_document_metadata_edit(&mut self) {
        let Some(ref doc) = self.active_document else {
            self.status_message = "No active document.".to_string();
            return;
        };
        let uri = self.doc_form_uri.trim().to_string();
        if uri.is_empty() {
            self.status_message = "URI is required.".to_string();
            return;
        }
        let collections: Vec<String> = self
            .doc_form_collections
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let quality = self.doc_form_quality.trim().parse::<i64>().ok();
        let Some(client) = &self.client else {
            self.status_message = "No server connected.".to_string();
            return;
        };
        let client = client.clone();
        let content = self.doc_form_content.clone();
        match self
            .rt
            .block_on(client.update_document(&uri, &content, Some(&collections), quality))
        {
            Ok(()) => {
                let mut updated = doc.clone();
                let uri_display = uri.clone();
                updated.uri = uri;
                updated.collections = collections;
                updated.quality = quality;
                updated.content = content;
                self.set_active_document(updated);
                self.status_message = format!("Updated document: {}", uri_display);
                self.mode = self
                    .previous_mode
                    .clone()
                    .unwrap_or(AppMode::FullScreenView);
                self.previous_mode = None;
                if !self.records.is_empty() {
                    self.fetch_list();
                }
            }
            Err(e) => {
                self.status_message = format!("Failed to update metadata: {}", e);
            }
        }
    }

    fn load_query_file(&mut self, path: PathBuf) -> Result<()> {
        let contents = load_query_file(&path)?;
        self.query_editor = Self::new_query_editor(editor_lines(&contents));
        if self.use_edtui {
            self.edtui_state = EditorState::new(Lines::from(contents.as_str()));
        }
        self.active_query_file = Some(path.clone());
        let restored_results = self.restore_query_results_for_path(&path);
        self.query_dirty = false;
        self.query_last_edit = None;
        self.focus_query_panel();
        self.status_message = match restored_results {
            Ok(Some(_count)) => String::new(),
            Ok(None) => String::new(),
            Err(e) => {
                self.clear_query_results_view();
                format!(
                    "Loaded query file: {} (failed to restore cached results: {})",
                    display_query_path(&path, &self.query_root_dir),
                    e
                )
            }
        };
        Ok(())
    }

    fn save_query_file_if_dirty(&mut self) -> Result<()> {
        if !self.query_dirty {
            return Ok(());
        }

        let Some(path) = self.active_query_file.clone() else {
            return Ok(());
        };

        let contents = self.query_text();
        save_query_file(&path, &contents)?;
        self.query_dirty = false;
        self.query_last_edit = None;
        self.status_message = format!(
            "Saved query file: {}",
            display_query_path(&path, &self.query_root_dir)
        );
        Ok(())
    }

    fn save_query_file_manually(&mut self) {
        let Some(path) = self.active_query_file.clone() else {
            self.status_message = "No query file selected.".to_string();
            return;
        };

        if !self.query_dirty {
            self.status_message = format!(
                "Query file already saved: {}",
                display_query_path(&path, &self.query_root_dir)
            );
            return;
        }

        if let Err(e) = self.save_query_file_if_dirty() {
            self.status_message = format!("Save error: {}", e);
        }
    }

    fn maybe_autosave(&mut self) {
        if should_autosave(
            self.query_dirty,
            self.query_last_edit,
            Instant::now(),
            self.query_autosave_interval,
        ) {
            if let Err(e) = self.save_query_file_if_dirty() {
                self.status_message = format!("Autosave failed: {}", e);
            }
        }
    }

    fn set_transient_status_message(&mut self, message: String, duration: Duration) {
        self.status_message = message.clone();
        self.transient_status_message = Some(message);
        self.transient_status_expires_at = Some(Instant::now() + duration);
    }

    fn log_status_message_if_changed(&mut self) {
        if self.status_message != self.last_logged_status_message {
            if !self.status_message.is_empty() {
                debug!(
                    mode = ?self.mode,
                    focus = ?self.focus,
                    message = %self.status_message,
                    "status message"
                );
            }
            self.last_logged_status_message = self.status_message.clone();
        }
    }

    fn clear_expired_status_message(&mut self) {
        let Some(expires_at) = self.transient_status_expires_at else {
            return;
        };

        if Instant::now() < expires_at {
            return;
        }

        if self
            .transient_status_message
            .as_ref()
            .is_some_and(|message| message == &self.status_message)
        {
            self.status_message.clear();
        }
        self.transient_status_message = None;
        self.transient_status_expires_at = None;
    }

    fn show_query_editor(&mut self) {
        if self.active_query_file.is_none() {
            if self.refresh_query_files().is_ok() {
                if let Some(path) = self.active_query_file.clone() {
                    if let Err(e) = self.load_query_file(path) {
                        self.status_message = e.to_string();
                    }
                }
            }
        }
        self.focus_query_panel();
        if self.active_query_file.is_none() {
            self.status_message = format!(
                "No supported query files found in {}",
                self.query_root_dir.display()
            );
        }
    }

    fn open_query_file_picker(&mut self) {
        match self.refresh_query_files() {
            Ok(()) => {
                if self.query_files.is_empty() {
                    self.status_message = format!(
                        "No supported query files found in {}",
                        self.query_root_dir.display()
                    );
                } else {
                    self.open_modal(AppMode::QueryFileSelect);
                }
            }
            Err(e) => {
                self.status_message = e.to_string();
            }
        }
    }

    fn rebuild_module_clone_filtered_uris(&mut self) {
        let filter = self.module_clone_filter_input.to_lowercase();
        self.module_clone_filtered_uris = if filter.is_empty() {
            self.module_clone_all_uris.clone()
        } else {
            self.module_clone_all_uris
                .iter()
                .filter(|uri| uri.to_lowercase().contains(&filter))
                .cloned()
                .collect()
        };

        if self.module_clone_filtered_uris.is_empty() {
            self.module_clone_list_state.select(None);
        } else {
            let selected = self.module_clone_list_state.selected().unwrap_or(0);
            let clamped = selected.min(self.module_clone_filtered_uris.len().saturating_sub(1));
            self.module_clone_list_state.select(Some(clamped));
        }
    }

    fn move_module_clone_selection(&mut self, delta: isize) {
        if self.module_clone_filtered_uris.is_empty() {
            self.module_clone_list_state.select(None);
            return;
        }

        let len = self.module_clone_filtered_uris.len() as isize;
        let current = self
            .module_clone_list_state
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.module_clone_list_state.select(Some(next));
    }

    fn selected_module_clone_uri(&self) -> Option<String> {
        let selected = self.module_clone_list_state.selected()?;
        self.module_clone_filtered_uris.get(selected).cloned()
    }

    fn clone_root_for_active_server_and_modules(&self, modules_database: &str) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        let server_label = self
            .config
            .active_server
            .clone()
            .or_else(|| self.client.as_ref().map(|c| c.server.name.clone()))?;
        Some(
            home.join(server_label)
                .join("CLONES")
                .join(modules_database),
        )
    }

    fn open_module_clone_picker(&mut self) {
        let Some(client) = self.client.clone() else {
            self.status_message = "No server connected.".to_string();
            return;
        };

        let Some(modules_db) = self.config.active_modules_database.clone() else {
            self.status_message =
                "No modules database selected. Use :servers and choose one under App Servers."
                    .to_string();
            return;
        };

        match self.rt.block_on(client.list_module_uris(&modules_db)) {
            Ok(uris) => {
                if uris.is_empty() {
                    self.status_message =
                        format!("No module URIs found in modules database '{}'.", modules_db);
                    return;
                }

                self.module_clone_all_uris = uris;
                self.module_clone_filter_input.clear();
                self.module_clone_list_state.select(Some(0));
                self.rebuild_module_clone_filtered_uris();
                self.open_modal(AppMode::ModuleCloneSelect);
            }
            Err(e) => {
                self.status_message = format!("Failed to load module URIs: {}", e);
            }
        }
    }

    fn clone_selected_module(&mut self) {
        let Some(uri) = self.selected_module_clone_uri() else {
            self.status_message = "No module selected to clone.".to_string();
            return;
        };

        let Some(modules_db) = self.config.active_modules_database.clone() else {
            self.status_message =
                "No modules database selected. Use :servers and choose one under App Servers."
                    .to_string();
            return;
        };

        let Some(client) = self.client.clone() else {
            self.status_message = "No server connected.".to_string();
            return;
        };

        let content = match self
            .rt
            .block_on(client.get_document_content_from_database(&uri, &modules_db))
        {
            Ok(content) => content,
            Err(e) => {
                self.status_message = format!("Failed to clone module '{}': {}", uri, e);
                return;
            }
        };

        let Some(clone_root) = self.clone_root_for_active_server_and_modules(&modules_db) else {
            self.status_message =
                "Could not resolve clone root (missing home directory or active server)."
                    .to_string();
            return;
        };

        let relative_uri = uri.trim_start_matches('/');
        if relative_uri.is_empty() {
            self.status_message = format!("Invalid module URI: {}", uri);
            return;
        }

        let destination = clone_root.join(relative_uri);
        if let Some(parent) = destination.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                self.status_message = format!("Failed to create clone directory: {}", e);
                return;
            }
        }

        if let Err(e) = fs::write(&destination, content) {
            self.status_message = format!("Failed to write clone file: {}", e);
            return;
        }

        match self.switch_query_root(clone_root.clone()) {
            Ok(()) => {
                self.mode = AppMode::Normal;
                self.status_message =
                    format!("Cloned {} to {}", uri, display_folder_path(&destination));
            }
            Err(e) => {
                self.mode = AppMode::Normal;
                self.status_message = format!(
                    "Cloned {} to {}, but failed to switch folder: {}",
                    uri,
                    display_folder_path(&destination),
                    e
                );
            }
        }
    }

    fn status_line(&self) -> Line<'static> {
        let mode_label = format!(" {} ", self.status_mode_label());
        let mode_span = Span::styled(mode_label, self.edit_mode_style());
        let mut spans = vec![Span::raw(" "), mode_span];
        spans.extend(self.status_identity_spans());
        Line::from(spans)
    }

    fn execute_command(&mut self) {
        let previous_mode = self.mode.clone();
        let mut cmd = self.command_input.trim().to_string();
        self.command_input.clear();

        if cmd.is_empty() {
            return;
        }

        if self.mode == AppMode::StartPage {
            self.mode = AppMode::Normal;
        }

        if !cmd.starts_with(':') && self.focus == Focus::Command {
            cmd = format!(":{}", cmd);
        }

        if !cmd.starts_with(':') {
            // It's a filter for current results
            self.status_message = format!("Filter: {}", cmd);
            return;
        }

        let parts: Vec<&str> = cmd[1..].splitn(2, ' ').collect();
        let command = parts[0];
        let _arg = parts.get(1).map(|s| s.trim());

        debug!(command = %command, "executing command");

        match command {
            "servers" => self.cmd_servers(),
            "server-add" => self.open_server_add(),
            "databases" => self.cmd_databases(),
            "list" => {
                if let Err(e) = self.save_query_file_if_dirty() {
                    self.results_text = format!("Save error: {}", e);
                    return;
                }
                self.current_collection = None;
                self.uri_filter = None;
                self.filter_input.clear();
                self.current_page = 0;
                self.query_visible = false;
                self.focus_results_panel();
                self.fetch_list();
            }
            "collections" => self.cmd_collections(),
            "clear" => {
                self.current_collection = None;
                self.uri_filter = None;
                self.filter_input.clear();
                self.current_page = 0;
                self.status_message = "Cleared collection filter, URI filter, and page".to_string();
            }
            "tdes" => self.cmd_tdes(),
            "query" => self.show_query_editor(),
            "query-files" | "query-open" => self.open_query_file_picker(),
            "folders" => self.open_tracked_folder_picker(),
            "logs" => self.open_log_viewer(),
            "quit" => {
                if let Err(e) = self.save_query_file_if_dirty() {
                    self.results_text = format!("Save error: {}", e);
                    return;
                }
                self.should_quit = true;
            }
            _ => {
                // Check if it's list:collection-name
                if command.starts_with("list:") {
                    let col = &command[5..];
                    self.current_collection = Some(col.to_string());
                    self.current_page = 0;
                    self.fetch_list();
                } else {
                    warn!(command = %command, "unknown command");
                    self.mode = previous_mode;
                    self.set_transient_status_message(
                        format!("Unknown command: :{}", command),
                        Duration::from_secs(5),
                    );
                }
            }
        }
    }

    fn reconnect(&mut self) {
        self.client = Self::build_client_from_config(&self.config);
    }

    fn cmd_servers(&mut self) {
        self.open_servers_interface();
    }

    fn rebuild_server_list(&mut self) {
        let preferred = self
            .selected_server_name()
            .or_else(|| self.config.active_server.clone());

        self.server_list = self
            .config
            .servers
            .iter()
            .map(|s| {
                let active = if self.config.active_server.as_deref() == Some(&s.name) {
                    " (active)"
                } else {
                    ""
                };
                format!("{}{} - {}:{}", s.name, active, s.host, s.port)
            })
            .collect();

        let selected = preferred
            .as_ref()
            .and_then(|name| self.config.servers.iter().position(|s| &s.name == name))
            .or_else(|| {
                if self.config.servers.is_empty() {
                    None
                } else {
                    Some(0)
                }
            });
        self.server_list_state.select(selected);
    }

    fn selected_server_name(&self) -> Option<String> {
        let selected = self.server_list_state.selected()?;
        self.config.servers.get(selected).map(|s| s.name.clone())
    }

    fn move_server_selection(&mut self, delta: isize) {
        if self.config.servers.is_empty() {
            self.server_list_state.select(None);
            return;
        }

        let len = self.config.servers.len() as isize;
        let current = self
            .server_list_state
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.server_list_state.select(Some(next));
    }

    fn open_servers_interface(&mut self) {
        self.rebuild_server_list();
        self.refresh_servers_interface_data();
        self.servers_interface_focus = ServersInterfaceFocus::Servers;
        self.open_interface(AppInterface::Servers);
        if self.config.servers.is_empty() {
            self.status_message = "No servers configured. Press 'a' to add one.".to_string();
        }
    }

    fn refresh_servers_interface_data(&mut self) {
        self.refresh_database_list_for_interface();
        self.refresh_app_server_list_for_interface();
    }

    fn refresh_database_list_for_interface(&mut self) {
        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.list_databases()) {
                Ok(dbs) => {
                    self.database_list = dbs;
                    let selected = self
                        .config
                        .active_database
                        .as_ref()
                        .and_then(|active| self.database_list.iter().position(|d| d == active))
                        .or_else(|| {
                            if self.database_list.is_empty() {
                                None
                            } else {
                                Some(0)
                            }
                        });
                    self.database_list_state.select(selected);
                }
                Err(e) => {
                    self.database_list.clear();
                    self.database_list_state.select(None);
                    self.status_message = format!("Error listing databases: {}", e);
                }
            }
        } else {
            self.database_list.clear();
            self.database_list_state.select(None);
        }
    }

    fn refresh_app_server_list_for_interface(&mut self) {
        if let Some(active_server_name) = self.config.active_server.as_deref() {
            if let Some(server) = self.config.servers.iter().find(|s| s.name == active_server_name) {
                self.app_server_list = server.app_servers.clone();
            } else {
                self.app_server_list.clear();
            }
        } else {
            self.app_server_list.clear();
        }

        let selected = self
            .config
            .active_app_server
            .as_ref()
            .and_then(|active| {
                self.app_server_list.iter().position(|s| &s.name == active)
            })
            .or_else(|| {
                if self.app_server_list.is_empty() {
                    None
                } else {
                    Some(0)
                }
            });
        self.app_server_list_state.select(selected);
    }

    fn move_database_selection(&mut self, delta: isize) {
        if self.database_list.is_empty() {
            self.database_list_state.select(None);
            return;
        }

        let len = self.database_list.len() as isize;
        let current = self
            .database_list_state
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.database_list_state.select(Some(next));
    }

    fn move_app_server_selection(&mut self, delta: isize) {
        if self.app_server_list.is_empty() {
            self.app_server_list_state.select(None);
            return;
        }

        let len = self.app_server_list.len() as isize;
        let current = self
            .app_server_list_state
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.app_server_list_state.select(Some(next));
    }

    fn activate_selected_database(&mut self) {
        let Some(selected) = self.database_list_state.selected() else {
            self.status_message = "No database selected.".to_string();
            return;
        };
        let Some(db_name) = self.database_list.get(selected).cloned() else {
            self.status_message = "No database selected.".to_string();
            return;
        };

        self.config.active_database = Some(db_name.clone());
        match self.config.save() {
            Ok(()) => {
                if let Some(c) = &mut self.client {
                    c.set_database(db_name.clone());
                }
                self.current_collection = None;
                self.current_page = 0;
                self.status_message = format!("Database: {}", db_name);
            }
            Err(e) => {
                self.status_message = format!("Failed to save active database: {}", e);
            }
        }
    }

    fn activate_selected_app_server(&mut self) {
        let Some(selected) = self.app_server_list_state.selected() else {
            self.status_message = "No app server selected.".to_string();
            return;
        };
        let Some(app_server) = self.app_server_list.get(selected).cloned() else {
            self.status_message = "No app server selected.".to_string();
            return;
        };

        if let Some(active_server_name) = self.config.active_server.clone()
            && let Some(server) = self
                .config
                .servers
                .iter_mut()
                .find(|server| server.name == active_server_name)
        {
            server.port = app_server.port;
            server.secure = app_server.secure;
        }

        self.config.active_app_server = Some(app_server.name.clone());
        if let Some(content_db) = app_server.content_database.clone() {
            self.config.active_database = Some(content_db);
        }
        if let Some(modules_db) = app_server.modules_database.clone() {
            self.config.active_modules_database = Some(modules_db);
        }

        match self.config.save() {
            Ok(()) => {
                self.reconnect();
                self.refresh_servers_interface_data();
                if let Some(c) = &mut self.client
                    && let Some(db_name) = self.config.active_database.clone()
                {
                    c.set_database(db_name);
                }
                if let Some(c) = &mut self.client
                    && let Some(modules_db) = self.config.active_modules_database.clone()
                {
                    c.set_modules_database(modules_db);
                }
                self.status_message = format!(
                    "App server: {}:{} ({})",
                    app_server.name,
                    app_server.port,
                    if app_server.secure { "https" } else { "http" }
                );
            }
            Err(e) => {
                self.status_message = format!("Failed to save active app server: {}", e);
            }
        }
    }

    fn cycle_auth_type_next(&mut self) {
        let current = self.server_form_fields[4].parse::<AuthType>().unwrap_or(AuthType::Digest);
        let idx = AuthType::VARIANTS.iter().position(|v| v == &current).unwrap_or(0);
        let next = AuthType::VARIANTS[(idx + 1) % AuthType::VARIANTS.len()];
        self.server_form_fields[4] = next.to_string();
    }

    fn cycle_auth_type_prev(&mut self) {
        let current = self.server_form_fields[4].parse::<AuthType>().unwrap_or(AuthType::Digest);
        let idx = AuthType::VARIANTS.iter().position(|v| v == &current).unwrap_or(0);
        let prev = if idx == 0 {
            AuthType::VARIANTS[AuthType::VARIANTS.len() - 1]
        } else {
            AuthType::VARIANTS[idx - 1]
        };
        self.server_form_fields[4] = prev.to_string();
    }

    fn cycle_insecure_flag(&mut self) {
        self.server_form_fields[5] = if self.server_form_fields[5].eq_ignore_ascii_case("y") {
            "n".to_string()
        } else {
            "y".to_string()
        };
    }

    fn open_app_server_list(&mut self) {
        let Some(selected) = self.server_list_state.selected() else {
            self.status_message = "No server selected.".to_string();
            return;
        };
        let Some(server) = self.config.servers.get(selected) else {
            self.status_message = "No server selected.".to_string();
            return;
        };

        self.app_server_list_server_name = Some(server.name.clone());
        self.app_server_list = server.app_servers.clone();
        self.app_server_list_state.select(None);
        self.mode = AppMode::AppServerList;
    }

    fn open_app_server_add(&mut self) {
        self.app_server_form_mode = ServerFormMode::Add;
        self.app_server_form_step = 0;
        self.app_server_form_fields = vec![String::new(); 5];
        self.app_server_form_fields[2] = "n".to_string();
        self.app_server_edit_target = None;
        self.mode = AppMode::AppServerForm;
    }

    fn open_app_server_edit(&mut self) {
        let Some(selected) = self.app_server_list_state.selected() else {
            self.status_message = "No app server selected.".to_string();
            return;
        };
        let Some(endpoint) = self.app_server_list.get(selected) else {
            self.status_message = "No app server selected.".to_string();
            return;
        };

        self.app_server_form_mode = ServerFormMode::Edit;
        self.app_server_form_step = 0;
        self.app_server_form_fields = vec![
            endpoint.name.clone(),
            endpoint.port.to_string(),
            if endpoint.secure { "y".to_string() } else { "n".to_string() },
            endpoint.content_database.clone().unwrap_or_default(),
            endpoint.modules_database.clone().unwrap_or_default(),
        ];
        self.app_server_edit_target = Some(endpoint.name.clone());
        self.mode = AppMode::AppServerForm;
    }

    fn submit_app_server_form(&mut self) {
        let name = self.app_server_form_fields[0].trim().to_string();
        let port_input = self.app_server_form_fields[1].trim();
        let secure_input = self.app_server_form_fields[2].trim();
        let content_db = self.app_server_form_fields[3].trim();
        let modules_db = self.app_server_form_fields[4].trim();

        if name.is_empty() {
            self.status_message = "App server name is required.".to_string();
            return;
        }

        let port = match port_input.parse::<u16>() {
            Ok(p) => p,
            Err(_) => {
                self.status_message = "Port must be a number from 0 to 65535.".to_string();
                return;
            }
        };

        let secure = secure_input.eq_ignore_ascii_case("y") || secure_input.eq_ignore_ascii_case("yes");

        let Some(ref server_name) = self.app_server_list_server_name else {
            self.status_message = "No server context.".to_string();
            return;
        };

        let endpoint = AppServerEndpoint {
            name: name.clone(),
            port,
            secure,
            content_database: if content_db.is_empty() { None } else { Some(content_db.to_string()) },
            modules_database: if modules_db.is_empty() { None } else { Some(modules_db.to_string()) },
        };

        let was_edit = self.app_server_form_mode == ServerFormMode::Edit;

        if let Some(server) = self.config.servers.iter_mut().find(|s| &s.name == server_name) {
            // Remove old if editing and name changed
            if was_edit {
                if let Some(ref old_name) = self.app_server_edit_target {
                    if old_name != &name {
                        server.app_servers.retain(|a| a.name != *old_name);
                    } else {
                        server.app_servers.retain(|a| a.name != name);
                    }
                }
            } else {
                server.app_servers.retain(|a| a.name != name);
            }
            server.app_servers.push(endpoint);
            server.app_servers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }

        match self.config.save() {
            Ok(()) => {
                // Update the displayed list
                if let Some(server) = self.config.servers.iter().find(|s| &s.name == server_name) {
                    self.app_server_list = server.app_servers.clone();
                }
                self.app_server_edit_target = None;
                self.mode = AppMode::AppServerList;
                self.status_message = if was_edit {
                    format!("Updated app server: {}", name)
                } else {
                    format!("Added app server: {}", name)
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to save app server: {}", e);
            }
        }
    }

    fn delete_app_server(&mut self) {
        let Some(selected) = self.app_server_list_state.selected() else {
            self.status_message = "No app server selected.".to_string();
            return;
        };
        let Some(endpoint) = self.app_server_list.get(selected).cloned() else {
            self.status_message = "No app server selected.".to_string();
            return;
        };

        let Some(ref server_name) = self.app_server_list_server_name else {
            return;
        };

        if let Some(server) = self.config.servers.iter_mut().find(|s| &s.name == server_name) {
            server.app_servers.retain(|a| a.name != endpoint.name);
        }

        match self.config.save() {
            Ok(()) => {
                if let Some(server) = self.config.servers.iter().find(|s| &s.name == server_name) {
                    self.app_server_list = server.app_servers.clone();
                }
                self.app_server_list_state.select(None);
                self.mode = AppMode::AppServerList;
                self.status_message = format!("Removed app server: {}", endpoint.name);
            }
            Err(e) => {
                self.status_message = format!("Failed to remove app server: {}", e);
            }
        }
    }

    fn refresh_app_servers_from_discovery(&mut self) {
        let Some(ref server_name) = self.app_server_list_server_name else {
            return;
        };

        let Some(server_config) = self.config.servers.iter().find(|s| &s.name == server_name).cloned() else {
            return;
        };

        let client = match MarkLogicClient::new(server_config) {
            Ok(c) => c,
            Err(e) => {
                self.status_message = format!("Failed to create client: {}", e);
                return;
            }
        };
        match self.rt.block_on(client.list_app_servers()) {
            Ok(discovered) => {
                if let Some(server) = self.config.servers.iter_mut().find(|s| &s.name == server_name) {
                    for disc in discovered {
                        if let Some(existing) = server.app_servers.iter_mut().find(|a| a.name == disc.name) {
                            existing.port = disc.port;
                            existing.content_database = disc.content_database;
                            existing.modules_database = disc.modules_database;
                            // Only override secure if discovery found explicit SSL info
                            existing.secure = disc.secure;
                        } else {
                            server.app_servers.push(disc);
                        }
                    }
                    server.app_servers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                }
                if let Some(server) = self.config.servers.iter().find(|s| &s.name == server_name) {
                    self.app_server_list = server.app_servers.clone();
                }
                if let Err(e) = self.config.save() {
                    self.status_message = format!("Failed to save discovered app servers: {}", e);
                } else {
                    self.status_message = "Refreshed app servers from discovery.".to_string();
                }
            }
            Err(e) => {
                self.status_message = format!("Failed to discover app servers: {}", e);
            }
        }
    }

    fn open_server_add(&mut self) {
        if self.mode != AppMode::Interface(AppInterface::Servers) {
            self.open_servers_interface();
        }
        self.server_form_mode = ServerFormMode::Add;
        self.server_form_step = 0;
        self.server_form_fields = vec![String::new(); 6];
        self.server_form_fields[4] = "digest".to_string();
        self.server_form_fields[5] = "n".to_string();
        self.server_edit_target = None;
        self.mode = AppMode::ServerForm;
    }

    fn open_server_edit(&mut self) {
        let Some(selected) = self.server_list_state.selected() else {
            self.status_message = "No server selected.".to_string();
            return;
        };
        let Some(server) = self.config.servers.get(selected) else {
            self.status_message = "No server selected.".to_string();
            return;
        };

        self.server_form_mode = ServerFormMode::Edit;
        self.server_form_step = 0;
        self.server_form_fields = vec![
            server.name.clone(),
            server.host.clone(),
            server.username.clone(),
            server.password.clone(),
            server.auth_type.to_string(),
            if server.insecure { "y".to_string() } else { "n".to_string() },
        ];
        self.server_edit_target = Some(server.name.clone());
        self.mode = AppMode::ServerForm;
    }

    fn submit_server_form(&mut self) {
        let name = self.server_form_fields[0].trim().to_string();
        let host = self.server_form_fields[1].trim().to_string();
        let username = self.server_form_fields[2].trim().to_string();
        let password = self.server_form_fields[3].clone();
        let auth_input = self.server_form_fields[4].trim();
        let insecure_input = self.server_form_fields[5].trim();

        if name.is_empty() || host.is_empty() {
            self.status_message = "Name and host are required.".to_string();
            return;
        }

        let auth_type = if auth_input.is_empty() {
            AuthType::Digest
        } else {
            match AuthType::from_str(auth_input) {
                Ok(t) => t,
                Err(_) => {
                    self.status_message = "Auth type must be digest, basic, digestbasic, or application-level.".to_string();
                    return;
                }
            }
        };

        let insecure = insecure_input.eq_ignore_ascii_case("y") || insecure_input.eq_ignore_ascii_case("yes");

        let old_name = self.server_edit_target.clone();
        let was_edit = self.server_form_mode == ServerFormMode::Edit;
        let active_before = self.config.active_server.clone();
        let was_active = old_name
            .as_deref()
            .and_then(|target| {
                self.config
                    .active_server
                    .as_deref()
                    .map(|active| active == target)
            })
            .unwrap_or(false);

        let old_server = old_name
            .as_deref()
            .and_then(|old| self.config.servers.iter().find(|s| s.name == old).cloned());

        if let Some(old_name) = old_name.as_deref() && old_name != name {
            self.config.remove_server(old_name);
        }

        let is_new = !was_edit || old_name.as_deref() != Some(name.as_str());
        let app_servers = old_server
            .as_ref()
            .map(|s| s.app_servers.clone())
            .filter(|_| !is_new)
            .unwrap_or_else(|| vec![AppServerEndpoint::app_services_default()]);

        let (port, secure) = old_server
            .as_ref()
            .filter(|_| !is_new)
            .map(|s| (s.port, s.secure))
            .unwrap_or((8000, false));

        let server = ServerConfig {
            name: name.clone(),
            host,
            username,
            password,
            port,
            secure,
            auth_type,
            insecure,
            app_servers,
            uri: None,
        };
        let new_server_is_active = !was_edit;
        self.config.add_server(server);
        if new_server_is_active {
            self.config.active_server = Some(name.clone());
            self.config.active_app_server = Some("App-Services".to_string());
            self.config.active_database = Some("Documents".to_string());
            self.config.active_modules_database = Some("Modules".to_string());
        } else if was_edit && was_active {
            self.config.active_server = Some(name.clone());
        } else if was_edit {
            self.config.active_server = active_before;
        }

        match self.config.save() {
            Ok(()) => {
                self.reconnect();
                self.rebuild_server_list();
                if let Some(index) = self.config.servers.iter().position(|s| s.name == name) {
                    self.server_list_state.select(Some(index));
                }
                self.server_edit_target = None;
                self.mode = AppMode::Interface(AppInterface::Servers);
                self.status_message = if was_edit {
                    format!("Updated server: {}", name)
                } else {
                    format!("Added server: {}", name)
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to save server config: {}", e);
            }
        }
    }

    fn activate_selected_server(&mut self) {
        let Some(name) = self.selected_server_name() else {
            self.status_message = "No server selected.".to_string();
            return;
        };

        self.config.active_server = Some(name.clone());
        match self.config.save() {
            Ok(()) => {
                self.reconnect();
                self.current_collection = None;
                self.current_page = 0;
                self.rebuild_server_list();
                self.refresh_servers_interface_data();
                self.status_message = format!("Switched to server: {}", name);
            }
            Err(e) => {
                self.status_message = format!("Failed to save active server: {}", e);
            }
        }
    }

    fn start_delete_selected_server(&mut self) {
        self.server_delete_target = self.selected_server_name();
        if self.server_delete_target.is_some() {
            self.mode = AppMode::ServerDeleteConfirm;
        } else {
            self.status_message = "No server selected.".to_string();
        }
    }

    fn confirm_delete_selected_server(&mut self) {
        let Some(name) = self.server_delete_target.take() else {
            self.mode = AppMode::Interface(AppInterface::Servers);
            return;
        };

        self.config.remove_server(&name);
        match self.config.save() {
            Ok(()) => {
                self.reconnect();
                self.rebuild_server_list();
                self.status_message = format!("Removed server: {}", name);
            }
            Err(e) => {
                self.status_message = format!("Failed to remove server: {}", e);
            }
        }
        self.mode = AppMode::Interface(AppInterface::Servers);
    }

    fn cmd_databases(&mut self) {
        self.open_servers_interface();
        self.servers_interface_focus = ServersInterfaceFocus::Databases;
        if self.client.is_none() {
            self.status_message = "No server connected. Select or add a server first.".to_string();
        } else if self.database_list.is_empty() && self.status_message.is_empty() {
            self.status_message = "No databases returned for the active server.".to_string();
        }
    }

    fn cmd_collections(&mut self) {
        self.focus_results_panel();

        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.list_collections()) {
                Ok(cols) => {
                    self.collection_list = cols;
                    self.collection_list_state
                        .select(if self.collection_list.is_empty() {
                            None
                        } else {
                            Some(0)
                        });
                    self.open_modal(AppMode::CollectionSelect);
                }
                Err(e) => {
                    self.clear_query_results_view();
                    self.results_text = format!("Error listing collections: {}", e);
                }
            }
        } else {
            self.clear_query_results_view();
            self.results_text = "No server connected.".to_string();
        }
    }

    fn cmd_tdes(&mut self) {
        self.focus_results_panel();

        if let Some(client) = &self.client {
            let client = client.clone();
            let script = r#"
                'use strict';
                const tdes = fn.collection("http://marklogic.com/xdmp/tde");
                const results = [];
                for (const tde of tdes) {
                    results.push(xdmp.nodeUri(tde));
                }
                JSON.stringify(results);
            "#;
            match self.rt.block_on(client.js_query(script)) {
                Ok(parts) => {
                    let mut uris: Vec<String> = Vec::new();
                    for part in &parts {
                        if let Ok(arr) = serde_json::from_str::<Vec<String>>(part) {
                            uris.extend(arr);
                        }
                    }
                    if uris.is_empty() {
                        self.clear_query_results_view();
                        self.results_text = "No TDEs found.".to_string();
                    } else {
                        self.set_query_results(uris);
                        self.status_message = format!("{} TDE(s) found", self.query_results.len());
                    }
                }
                Err(e) => {
                    self.clear_query_results_view();
                    self.results_text = format!("Error listing TDEs: {}", e);
                }
            }
        } else {
            self.clear_query_results_view();
            self.results_text = "No server connected.".to_string();
        }
    }

    fn recalculate_page_size(&mut self) {
        if let Ok((_, rows)) = crossterm::terminal::size() {
            // status(3) + command(3) = 6 rows of chrome
            let main_height = rows.saturating_sub(6);
            let results_height = if self.query_visible {
                (main_height as f32 * 0.7) as u16
            } else {
                main_height
            };
            let usable = results_height.saturating_sub(4) as usize; // borders + header
            if usable > 0 {
                self.page_size = usable;
            }
        }
    }

    fn fetch_list(&mut self) {
        debug!(
            page = self.current_page,
            page_size = self.page_size,
            collection = ?self.current_collection,
            uri_filter = ?self.uri_filter,
            "fetching document list"
        );
        self.recalculate_page_size();

        // Enter document-list mode for the results pane and drop any stale query output.
        self.records.clear();
        self.list_state.select(None);
        self.selected_indices.clear();
        self.total_results = None;
        self.query_results.clear();
        self.query_results_timestamp = None;
        self.query_results_state.select(None);
        self.last_query_results_g = None;

        if let Some(client) = &self.client {
            let client = client.clone();
            let col = self.current_collection.as_deref();
            let dir = self.uri_filter.as_deref();
            let start = self.current_page * self.page_size + 1;
            match self
                .rt
                .block_on(client.search_documents(col, None, dir, start, self.page_size))
            {
                Ok(paged) => {
                    info!(
                        page = self.current_page,
                        page_size = self.page_size,
                        returned = paged.results.len(),
                        total = ?paged.total,
                        "fetched document list"
                    );
                    self.total_results = paged.total;
                    self.records = paged.results;
                    self.list_state.select(if self.records.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                    self.focus_results_panel();
                    self.results_text = if self.records.is_empty() {
                        "No documents found.".to_string()
                    } else {
                        String::new()
                    };
                    self.status_message = String::new();
                }
                Err(e) => {
                    error!(error = %e, "failed to fetch document list");
                    self.results_text = format!("Error: {}", e);
                }
            }
        } else {
            warn!("fetch_list requested without active client");
            self.results_text = "No server connected.".to_string();
        }
    }

    fn select_query_file(&mut self, path: PathBuf) {
        if let Err(e) = self.save_query_file_if_dirty() {
            self.results_text = format!("Save error: {}", e);
            return;
        }

        if let Err(e) = self.load_query_file(path) {
            self.results_text = format!("Load error: {}", e);
        }
    }

    fn execute_query(&mut self) {
        let Some(path) = self.active_query_file.clone() else {
            self.results_text = "No query file selected.".to_string();
            return;
        };

        if let Err(e) = self.save_query_file_if_dirty() {
            self.query_results.clear();
            self.results_text = format!("Save error: {}", e);
            return;
        }

        if let Some(client) = &self.client {
            let mut client = client.clone();
            let query = self.query_text();
            let modules_db_override = parse_modules_database_override(&query);
            if let Some(override_db) = modules_db_override.as_ref() {
                client.set_modules_database(override_db.clone());
                self.status_message = format!("Query modules DB override: {}", override_db);
            }
            debug!(
                file = %display_query_path(&path, &self.query_root_dir),
                bytes = query.len(),
                query = %query,
                modules_database = ?client.modules_database,
                modules_database_override = ?modules_db_override,
                "executing query"
            );
            let result = match query_execution_kind(&path) {
                QueryExecutionKind::JavaScript => self.rt.block_on(client.js_query(&query)),
                QueryExecutionKind::XQuery => self.rt.block_on(client.xquery_query(&query)),
                QueryExecutionKind::Deferred => {
                    self.query_results.clear();
                    self.results_text = format!(
                        "Running {} files is not implemented yet.",
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_or("this")
                    );
                    self.status_message = format!(
                        "Execution for {} files is deferred.",
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_or("this")
                    );
                    return;
                }
                QueryExecutionKind::Unsupported => {
                    self.query_results.clear();
                    self.results_text = format!(
                        "Unsupported query file type: {}",
                        display_query_path(&path, &self.query_root_dir)
                    );
                    return;
                }
            };

            match result {
                Ok(parts) => {
                    info!(
                        file = %display_query_path(&path, &self.query_root_dir),
                        parts = parts.len(),
                        "query executed successfully"
                    );
                    self.set_query_results(parts);
                    self.persist_query_results_for_path(&path);
                }
                Err(e) => {
                    error!(
                        file = %display_query_path(&path, &self.query_root_dir),
                        error = %e,
                        "query execution failed"
                    );
                    self.query_results.clear();
                    self.query_results_state.select(None);
                    let mut error_text = format!("Query error: {:#}", e);
                    let mut current = e.source();
                    while let Some(cause) = current {
                        error_text.push_str(&format!("\nCaused by: {:#}", cause));
                        current = cause.source();
                    }
                    self.results_text = error_text;
                }
            }
        } else {
            warn!("execute_query requested without active client");
            self.results_text = "No server connected.".to_string();
        }
    }

    fn delete_selected(&mut self) {
        if self.selected_indices.is_empty() {
            self.status_message = "No records selected for deletion.".to_string();
            return;
        }
        self.open_modal(AppMode::DeleteConfirm);
    }

    fn confirm_delete(&mut self) {
        let uris: Vec<String> = self
            .selected_indices
            .iter()
            .filter_map(|&i| self.records.get(i).map(|r| r.uri.clone()))
            .collect();
        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.delete_documents(&uris)) {
                Ok(()) => {
                    self.status_message = format!("Deleted {} documents.", uris.len());
                    self.selected_indices.clear();
                    self.fetch_list();
                }
                Err(e) => {
                    self.status_message = format!("Delete error: {}", e);
                }
            }
        }
        self.close_modal_with_focus(Focus::Results);
    }

    fn delete_uris(&self) -> Vec<String> {
        self.selected_indices
            .iter()
            .filter_map(|&i| self.records.get(i).map(|r| r.uri.clone()))
            .collect()
    }

    fn open_record(&mut self) {
        if let Some(idx) = self.list_state.selected() {
            if let Some(record) = self.records.get(idx) {
                let uri = record.uri.clone();
                if let Some(client) = &self.client {
                    let client = client.clone();
                    match self.rt.block_on(client.get_document(&uri)) {
                        Ok(detail) => {
                            self.set_active_document(detail);
                        }
                        Err(e) => {
                            self.active_document = None;
                            self.full_view_content = format!("Error loading document: {}", e);
                            self.full_view_scroll = 0;
                            self.mode = AppMode::FullScreenView;
                        }
                    }
                } else {
                    self.active_document = None;
                    self.full_view_content = format!("URI: {}", uri);
                    self.full_view_scroll = 0;
                    self.mode = AppMode::FullScreenView;
                }
            }
        }
    }
}

fn parse_modules_database_override(query: &str) -> Option<String> {
    const PREFIX: &str = "(:~modules-database:";
    for line in query.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(PREFIX)
            && let Some((db, suffix)) = rest.split_once(":)")
            && suffix.trim().is_empty()
        {
            let db = db.trim();
            if !db.is_empty() {
                return Some(db.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        App, AppInterface, AppMode, EditMode, Focus, LOG_FILE_PATH, ServerFormMode,
        ServersInterfaceFocus, TrackedFolderEntry, TrackedFolderMenuItem, display_folder_path,
        handle_command_key, handle_filter_key, handle_fullscreen_key, handle_log_viewer_key,
        handle_navigation_mode_key, handle_query_insert_transition_key, handle_query_key,
        handle_results_key, handle_return_to_start_page_key, handle_server_delete_confirm_key,
        handle_servers_interface_key, map_query_navigation_key, parse_modules_database_override,
        resolve_folder_input, temp_editor_path,
    };
    use crate::client::{AppServerEndpoint, AuthType, ServerConfig};
    use crate::config::AppConfig;
    use crate::tracked_folder::TrackedFolderStore;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use edtui::{EditorEventHandler, EditorState};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::{ListState, TableState};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{Duration, SystemTime},
    };
    use tokio::runtime::Runtime;

    fn test_app() -> App {
        App {
            config: AppConfig::default(),
            client: None,
            focus: Focus::Results,
            edit_mode: EditMode::Navigate,
            mode: AppMode::Normal,
            previous_mode: None,
            modal_origin_focus: None,
            interface_origin_focus: None,
            command_input: String::new(),
            query_editor: App::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            should_quit: false,
            query_root_dir: PathBuf::from("."),
            query_result_cache_dir: PathBuf::from(".marklogic-tui"),
            query_files: Vec::new(),
            active_query_file: None,
            query_file_list_state: ListState::default(),
            module_clone_all_uris: Vec::new(),
            module_clone_filtered_uris: Vec::new(),
            module_clone_filter_input: String::new(),
            module_clone_list_state: ListState::default(),
            tracked_folders: TrackedFolderStore::default(),
            tracked_folder_items: Vec::new(),
            tracked_folder_list_state: ListState::default(),
            tracked_folder_input: String::new(),
            tracked_folder_delete_target: None,
            tracked_folder_cache_clear_target: None,
            file_list_visible: true,
            file_delete_target: None,
            rename_file_input: String::new(),
            rename_file_target: None,
            new_query_file_input: String::new(),
            query_dirty: false,
            query_last_edit: None,
            query_autosave_interval: Duration::from_secs(2),
            results_text: String::new(),
            status_message: String::new(),
            transient_status_message: None,
            transient_status_expires_at: None,
            query_results: Vec::new(),
            query_results_timestamp: None,
            query_results_state: TableState::default(),
            records: Vec::new(),
            list_state: TableState::default(),
            selected_indices: Vec::new(),
            current_page: 0,
            page_size: 20,
            total_results: None,
            current_collection: None,
            uri_filter: None,
            filter_input: String::new(),
            last_esc: None,
            last_query_results_g: None,
            active_document: None,
            full_view_content: String::new(),
            full_view_scroll: 0,
            log_file_path: PathBuf::from(LOG_FILE_PATH),
            log_view_content: String::new(),
            log_view_scroll: 0,
            log_viewer_area_height: 0,
            last_log_viewer_g: None,
            last_logged_status_message: String::new(),
            server_form_mode: ServerFormMode::Add,
            server_form_step: 0,
            server_form_fields: vec![String::new(); 6],
            server_edit_target: None,
            server_delete_target: None,
            servers_interface_focus: ServersInterfaceFocus::Servers,
            app_server_form_mode: ServerFormMode::Add,
            app_server_form_step: 0,
            app_server_form_fields: vec![String::new(); 5],
            app_server_edit_target: None,
            app_server_delete_target: None,
            app_server_list_server_name: None,
            doc_form_step: 0,
            doc_form_uri: String::new(),
            doc_form_collections: String::new(),
            doc_form_quality: "0".to_string(),
            doc_form_content: String::new(),
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
            database_list: Vec::new(),
            database_list_state: ListState::default(),
            app_server_list: Vec::new(),
            app_server_list_state: ListState::default(),
            server_list: Vec::new(),
            server_list_state: ListState::default(),
            collection_list: Vec::new(),
            collection_list_state: ListState::default(),
            fullscreen_area_height: 0,
            last_fullscreen_g: None,
            rt: Runtime::new().unwrap(),
            use_edtui: false,
            edtui_state: EditorState::default(),
            edtui_handler: EditorEventHandler::default(),
        }
    }

    #[test]
    fn build_client_from_config_applies_modules_database() {
        let mut config = AppConfig::default();
        let mut server = test_server("local", "localhost");
        server.app_servers.push(AppServerEndpoint {
            name: "App-Services".to_string(),
            port: 8010,
            secure: false,
            content_database: Some("Documents".to_string()),
            modules_database: Some("Modules".to_string()),
        });
        config.servers = vec![server];
        config.active_server = Some("local".to_string());
        config.active_app_server = Some("App-Services".to_string());
        config.active_database = Some("Documents".to_string());
        config.active_modules_database = Some("Modules".to_string());

        let client = App::build_client_from_config(&config).expect("client should build");
        assert_eq!(client.database.as_deref(), Some("Documents"));
        assert_eq!(client.modules_database.as_deref(), Some("Modules"));
        assert_eq!(client.server.port, 8010);
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "marklogic-tui-main-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    fn test_server(name: &str, host: &str) -> ServerConfig {
        ServerConfig {
            name: name.to_string(),
            host: host.to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8000,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: vec![],
            uri: None,
        }
    }

    #[test]
    fn temp_editor_path_preserves_active_file_extension() {
        let temp_path = temp_editor_path(Some(Path::new("query.xqy")));
        assert_eq!(
            temp_path.extension().and_then(|ext| ext.to_str()),
            Some("xqy")
        );
    }

    #[test]
    fn temp_editor_path_falls_back_to_tmp_without_active_extension() {
        let temp_path = temp_editor_path(None);
        assert_eq!(
            temp_path.extension().and_then(|ext| ext.to_str()),
            Some("tmp")
        );
    }

    #[test]
    fn modal_input_numeric_shortcuts_switch_panels() {
        let mut app = test_app();
        app.focus = Focus::Results;

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Query);
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert!(app.query_visible);

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.edit_mode, EditMode::Navigate);

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Command);
        assert_eq!(app.edit_mode, EditMode::Navigate);
    }

    #[test]
    fn modal_input_numeric_shortcuts_do_not_hijack_command_input() {
        let mut app = test_app();
        app.focus = Focus::Command;

        assert!(!handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Command);
    }

    #[test]
    fn modal_input_command_tab_cycles_when_input_is_empty() {
        let mut app = test_app();
        app.focus = Focus::Command;

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn modal_input_command_tab_keeps_completion_once_typing_starts() {
        let mut app = test_app();
        app.focus = Focus::Command;
        app.command_input = ":q".to_string();
        app.update_autocomplete();

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Command);
        assert_eq!(app.command_input, ":query");
    }

    #[test]
    fn modal_input_command_escape_closes_and_focuses_next_panel() {
        let mut app = test_app();
        app.focus = Focus::Command;
        app.command_input = ":q".to_string();
        app.update_autocomplete();

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Query);
        assert!(app.command_input.is_empty());
        assert!(app.autocomplete_suggestions.is_empty());
    }

    #[test]
    fn start_page_escape_keeps_command_focus() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus = Focus::Command;
        app.command_input = ":q".to_string();
        app.update_autocomplete();

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.focus, Focus::Command);
        assert!(app.command_input.is_empty());
        assert!(app.autocomplete_suggestions.is_empty());
    }

    #[test]
    fn start_page_execute_command_switches_to_normal_mode() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus = Focus::Command;
        app.command_input = ":clear".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn unknown_command_sets_transient_status_without_touching_results() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_results_panel();
        app.results_text = "existing results".to_string();
        app.command_input = ":bogus".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.results_text, "existing results");
        assert_eq!(app.status_message, "Unknown command: :bogus");
        assert_eq!(
            app.transient_status_message.as_deref(),
            Some("Unknown command: :bogus")
        );
        assert!(app.transient_status_expires_at.is_some());
    }

    #[test]
    fn unknown_command_from_start_page_stays_on_start_page() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus_command_panel();
        app.command_input = ":bogus".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.status_message, "Unknown command: :bogus");
    }

    #[test]
    fn transient_status_clears_after_expiry() {
        let mut app = test_app();
        app.set_transient_status_message("Unknown command: :bogus".to_string(), Duration::ZERO);

        app.clear_expired_status_message();

        assert!(app.status_message.is_empty());
        assert!(app.transient_status_message.is_none());
        assert!(app.transient_status_expires_at.is_none());
    }

    #[test]
    fn quit_command_sets_quit_flag() {
        let mut app = test_app();
        app.command_input = ":quit".to_string();

        app.execute_command();

        assert!(app.should_quit);
    }

    #[test]
    fn quit_command_leaves_start_page_and_sets_quit_flag() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus = Focus::Command;
        app.command_input = ":quit".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.should_quit);
    }

    #[test]
    fn enter_start_page_resets_focus_and_command_state() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus = Focus::Results;
        app.query_visible = true;
        app.command_input = ":list".to_string();
        app.autocomplete_suggestions = vec![":list"];
        app.autocomplete_selected = 0;

        app.enter_start_page();

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.focus, Focus::Command);
        assert!(!app.query_visible);
        assert!(app.command_input.is_empty());
        assert!(app.autocomplete_suggestions.is_empty());
        assert_eq!(app.autocomplete_selected, 0);
    }

    #[test]
    fn q_in_navigation_mode_returns_to_start_page() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_results_panel();
        app.query_visible = true;

        assert!(
            handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.focus, Focus::Command);
        assert!(!app.query_visible);
    }

    #[test]
    fn q_in_insert_mode_does_not_return_to_start_page() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_query_panel();
        app.edit_mode = EditMode::Insert;

        assert!(
            !handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.edit_mode, EditMode::Insert);
    }

    #[test]
    fn q_in_command_input_remains_text_input() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_command_panel();

        assert!(
            !handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Command);
    }

    #[test]
    fn modal_input_insert_mode_can_be_entered_and_exited() {
        let mut app = test_app();
        app.focus_query_panel();

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
        ));
        assert_eq!(app.edit_mode, EditMode::Insert);
        assert_eq!(app.focus, Focus::Query);

        app.exit_insert_mode();
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn modal_input_ctrl_t_exits_insert_and_cycles_panel() {
        let mut app = test_app();
        app.focus_query_panel();
        app.edit_mode = EditMode::Insert;

        assert!(handle_query_insert_transition_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn modal_input_filter_returns_to_results_on_escape() {
        let mut app = test_app();
        app.focus_results_panel();

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Filter);
        assert_eq!(app.edit_mode, EditMode::Navigate);

        handle_filter_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.edit_mode, EditMode::Navigate);
    }

    #[test]
    fn modal_input_i_only_enters_insert_in_query_panel() {
        let mut app = test_app();
        app.focus = Focus::Command;

        assert!(!handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
        ));
        assert_eq!(app.edit_mode, EditMode::Navigate);

        app.focus_query_panel();
        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
        ));
        assert_eq!(app.edit_mode, EditMode::Insert);
    }

    #[test]
    fn modal_input_tab_cycles_panels_in_navigation_mode() {
        let mut app = test_app();
        app.focus_query_panel();

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Results);

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn modal_input_query_navigation_maps_vim_keys() {
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .is_none()
        );
    }

    #[test]
    fn modal_input_query_title_reflects_mode_specific_shortcuts() {
        let mut app = test_app();
        app.focus_query_panel();
        // query_title returns the file name with optional dirty marker
        assert!(app.query_title().starts_with("Query: "));

        app.active_query_file = Some(PathBuf::from("test.xqy"));
        app.query_dirty = true;
        assert_eq!(app.query_title(), "Query: test.xqy*");
    }

    #[test]
    fn modal_input_new_query_file_default_uses_current_extension() {
        let mut app = test_app();
        let temp_dir = std::env::temp_dir().join(format!(
            "marklogic-tui-query-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("query-1.xqy"), "").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_files = vec![temp_dir.join("query-1.xqy")];
        app.active_query_file = Some(temp_dir.join("current.xqy"));

        assert_eq!(app.default_new_query_file_name(), "query-2.xqy");

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn modal_input_new_query_file_defaults_to_xqy_without_active_file() {
        let mut app = test_app();
        let temp_dir = std::env::temp_dir().join(format!(
            "marklogic-tui-query-default-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();
        app.query_root_dir = temp_dir.clone();

        assert_eq!(app.default_new_query_file_name(), "query-1.xqy");

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn modal_input_new_query_file_rejects_unsupported_extensions() {
        let mut app = test_app();
        app.mode = AppMode::QueryFileCreate;
        app.new_query_file_input = "query-1.txt".to_string();

        app.create_new_query_file_from_input();

        assert_eq!(app.mode, AppMode::QueryFileCreate);
        assert!(
            app.status_message
                .contains("Unsupported query file extension")
        );
    }

    #[test]
    fn resolve_folder_input_supports_relative_and_home_paths() {
        let current_root = Path::new("/tmp/project");

        assert_eq!(
            resolve_folder_input(current_root, "nested"),
            Path::new("/tmp/project/nested")
        );

        if let Some(home_dir) = dirs::home_dir() {
            assert_eq!(resolve_folder_input(current_root, "~"), home_dir);
            assert_eq!(
                resolve_folder_input(current_root, "~/queries"),
                home_dir.join("queries")
            );
        }
    }

    #[test]
    fn rebuild_tracked_folder_items_groups_favorites_before_recents() {
        let mut app = test_app();
        app.query_root_dir = PathBuf::from("/tmp/current");
        app.tracked_folders = TrackedFolderStore {
            folders: vec![
                TrackedFolderEntry {
                    path: "/tmp/recent-a".to_string(),
                    favorite: false,
                    last_accessed: Some(2),
                },
                TrackedFolderEntry {
                    path: "/tmp/favorite".to_string(),
                    favorite: true,
                    last_accessed: Some(3),
                },
                TrackedFolderEntry {
                    path: "/tmp/recent-b".to_string(),
                    favorite: false,
                    last_accessed: Some(4),
                },
            ],
        };

        app.rebuild_tracked_folder_items();

        assert!(matches!(
            app.tracked_folder_items.first(),
            Some(TrackedFolderMenuItem::Header("Favorites"))
        ));
        assert!(matches!(
            app.tracked_folder_items.get(1),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/favorite"
        ));
        assert!(matches!(
            app.tracked_folder_items.get(2),
            Some(TrackedFolderMenuItem::Header("Recent"))
        ));
        assert!(matches!(
            app.tracked_folder_items.get(3),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/recent-b"
        ));
        assert!(matches!(
            app.tracked_folder_items.get(4),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/recent-a"
        ));
    }

    #[test]
    fn query_file_list_f_key_opens_tracked_folder_picker() {
        let mut app = test_app();
        app.focus_query_panel();
        app.file_list_visible = true;
        app.tracked_folders = TrackedFolderStore {
            folders: vec![TrackedFolderEntry {
                path: "/tmp/folder".to_string(),
                favorite: false,
                last_accessed: Some(1),
            }],
        };

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
        );

        assert_eq!(app.mode, AppMode::TrackedFolderSelect);
        assert!(matches!(
            app.tracked_folder_items.get(1),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/folder"
        ));
    }

    #[test]
    fn folders_command_opens_tracked_folder_picker() {
        let mut app = test_app();
        app.command_input = ":folders".to_string();
        app.tracked_folders = TrackedFolderStore {
            folders: vec![TrackedFolderEntry {
                path: "/tmp/folder".to_string(),
                favorite: false,
                last_accessed: Some(1),
            }],
        };

        app.execute_command();

        assert_eq!(app.mode, AppMode::TrackedFolderSelect);
        assert!(matches!(
            app.tracked_folder_items.get(1),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/folder"
        ));
    }

    #[test]
    fn switch_query_root_refreshes_supported_files_and_cache_dir() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("switch-query-root");
        let folder_one = temp_dir.join("one");
        let folder_two = temp_dir.join("two");
        fs::create_dir_all(&folder_one).unwrap();
        fs::create_dir_all(&folder_two).unwrap();
        fs::write(folder_two.join("query-2.xqy"), "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = folder_one.clone();
        app.tracked_folders = TrackedFolderStore::default();
        let canonical_folder_two = fs::canonicalize(&folder_two).unwrap();

        app.switch_query_root(folder_two.clone()).unwrap();

        assert_eq!(app.query_root_dir, canonical_folder_two);
        assert_eq!(
            app.query_result_cache_dir,
            app.query_root_dir.join(".marklogic-tui")
        );
        assert_eq!(
            app.query_files,
            vec![app.query_root_dir.join("query-2.xqy")]
        );
        assert_eq!(
            app.active_query_file,
            Some(app.query_root_dir.join("query-2.xqy"))
        );
        assert!(
            app.tracked_folders
                .folders
                .iter()
                .any(|entry| entry.path == display_folder_path(&app.query_root_dir))
        );

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn clear_selected_tracked_folder_cache_removes_marklogic_tui_dir() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("clear-folder-cache");
        let folder = temp_dir.join("queries");
        let cache_dir = folder.join(".marklogic-tui");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("results-v1.json"), "{}").unwrap();
        app.tracked_folder_cache_clear_target = Some(folder.clone());

        app.confirm_clear_selected_tracked_folder_cache();

        assert!(!cache_dir.exists());
        assert_eq!(app.mode, AppMode::TrackedFolderSelect);

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn modal_input_setting_query_results_keeps_query_focus() {
        let mut app = test_app();
        app.focus_query_panel();

        app.set_query_results(vec!["result".to_string()]);

        assert_eq!(app.focus, Focus::Query);
        assert_eq!(app.query_results, vec!["result".to_string()]);
    }

    #[test]
    fn query_file_switch_restores_cached_results_per_file() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-cache-switch");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_one = temp_dir.join("query-1.xqy");
        let query_two = temp_dir.join("query-2.xqy");
        fs::write(&query_one, "xquery version \"1.0-ml\";").unwrap();
        fs::write(&query_two, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.query_files = vec![query_one.clone(), query_two.clone()];
        app.query_file_list_state.select(Some(0));

        app.set_query_results(vec!["result-a".to_string()]);
        app.persist_query_results_for_path(&query_one);

        app.set_query_results(vec!["result-b1".to_string(), "result-b2".to_string()]);
        app.query_results_state.select(Some(1));
        app.persist_query_results_for_path(&query_two);

        app.load_query_file(query_one.clone()).unwrap();
        assert_eq!(app.query_results, vec!["result-a".to_string()]);
        assert_eq!(app.query_results_state.selected(), Some(0));

        app.load_query_file(query_two.clone()).unwrap();
        assert_eq!(
            app.query_results,
            vec!["result-b1".to_string(), "result-b2".to_string()]
        );
        assert_eq!(app.query_results_state.selected(), Some(1));

        app.load_query_file(query_one).unwrap();
        assert_eq!(app.query_results, vec!["result-a".to_string()]);
        assert_eq!(app.query_results_state.selected(), Some(0));

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn query_result_cache_persists_timestamp() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-cache-timestamp");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_path = temp_dir.join("query-1.xqy");
        fs::write(&query_path, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");

        // Set results with a specific timestamp
        app.set_query_results(vec!["result".to_string()]);
        let before_save = app.query_results_timestamp.unwrap();
        app.persist_query_results_for_path(&query_path);

        // Clear and restore
        app.clear_query_results_view();
        assert!(app.query_results_timestamp.is_none());

        app.load_query_file(query_path).unwrap();
        assert_eq!(app.query_results, vec!["result".to_string()]);
        let after_restore = app.query_results_timestamp.unwrap();

        // Timestamp should be preserved (within 1 second tolerance)
        let diff = after_restore
            .duration_since(before_save)
            .unwrap_or_default()
            .as_secs();
        assert_eq!(
            diff, 0,
            "Timestamp should be preserved through cache round-trip"
        );

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn load_query_file_success_does_not_set_status_message() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-load-status-message");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_path = temp_dir.join("query-1.xqy");
        fs::write(&query_path, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");

        app.load_query_file(query_path.clone()).unwrap();
        assert!(app.status_message.is_empty());

        app.set_query_results(vec!["result".to_string()]);
        app.persist_query_results_for_path(&query_path);
        app.load_query_file(query_path).unwrap();
        assert!(app.status_message.is_empty());

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn load_query_file_without_cached_results_clears_previous_query_results() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-cache-miss");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_one = temp_dir.join("query-1.xqy");
        let query_two = temp_dir.join("query-2.xqy");
        fs::write(&query_one, "xquery version \"1.0-ml\";").unwrap();
        fs::write(&query_two, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.query_files = vec![query_one.clone(), query_two.clone()];

        app.set_query_results(vec!["cached-result".to_string()]);
        app.persist_query_results_for_path(&query_one);

        app.load_query_file(query_one).unwrap();
        assert_eq!(app.query_results, vec!["cached-result".to_string()]);
        assert_eq!(app.query_results_state.selected(), Some(0));

        app.load_query_file(query_two).unwrap();
        assert!(app.query_results.is_empty());
        assert_eq!(app.query_results_state.selected(), None);

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn fetch_list_clears_stale_query_results_before_list_mode() {
        let mut app = test_app();
        app.focus_results_panel();
        app.set_query_results(vec!["stale-query-result".to_string()]);
        app.results_text = "old query text".to_string();
        app.query_results_timestamp = Some(SystemTime::now());
        assert!(!app.query_results.is_empty());

        app.focus_results_panel();
        assert_eq!(app.focus, Focus::Results);

        app.fetch_list();

        assert!(app.query_results.is_empty());
        assert_eq!(app.query_results_state.selected(), None);
        assert_eq!(app.results_text, "No server connected.");
        assert_eq!(app.query_results_timestamp, None);
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn cmd_tdes_without_server_keeps_results_focus() {
        let mut app = test_app();
        app.focus = Focus::Command;

        app.cmd_tdes();

        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.results_text, "No server connected.");
    }

    #[test]
    fn cmd_collections_without_server_keeps_results_focus() {
        let mut app = test_app();
        app.focus = Focus::Command;

        app.cmd_collections();

        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.results_text, "No server connected.");
    }

    #[test]
    fn modal_cancel_restores_original_focus() {
        let mut app = test_app();
        app.focus_query_panel();
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn servers_command_opens_interface_even_when_empty() {
        let mut app = test_app();
        app.command_input = ":servers".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(app.server_list_state.selected(), None);
        assert!(
            app.status_message.contains("No servers configured"),
            "empty state should guide the user toward adding a server"
        );
    }

    #[test]
    fn servers_interface_navigation_supports_top_and_bottom() {
        let mut app = test_app();
        app.config.servers = vec![
            test_server("one", "one.example"),
            test_server("two", "two.example"),
            test_server("three", "three.example"),
        ];
        app.open_servers_interface();

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_list_state.selected(), Some(2));

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_list_state.selected(), Some(0));
    }

    #[test]
    fn server_add_validation_keeps_form_open_for_missing_uri() {
        let mut app = test_app();
        app.open_server_add();
        app.server_form_fields[0] = "local".to_string();

        app.submit_server_form();

        assert_eq!(app.mode, AppMode::ServerForm);
        assert!(app.config.servers.is_empty());
        assert_eq!(app.status_message, "Name and host are required.");
    }

    #[test]
    fn server_edit_prefills_selected_server() {
        let mut app = test_app();
        app.config.servers = vec![test_server("local", "localhost")];
        app.open_servers_interface();

        app.open_server_edit();

        assert_eq!(app.mode, AppMode::ServerForm);
        assert_eq!(app.server_form_mode, ServerFormMode::Edit);
        assert_eq!(app.server_edit_target.as_deref(), Some("local"));
        assert_eq!(app.server_form_fields[0], "local");
        assert_eq!(app.server_form_fields[1], "localhost");
        assert_eq!(app.server_form_fields[4], "digest");
        assert_eq!(app.server_form_fields[5], "n");
    }

    #[test]
    fn server_delete_cancel_returns_to_servers_interface() {
        let mut app = test_app();
        app.config.servers = vec![test_server("local", "localhost")];
        app.open_servers_interface();
        app.start_delete_selected_server();

        handle_server_delete_confirm_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert!(app.server_delete_target.is_none());
        assert_eq!(app.config.servers.len(), 1);
    }

    #[test]
    fn server_form_auth_type_cycles_forward() {
        let mut app = test_app();
        app.open_server_add();

        app.server_form_step = 4;
        assert_eq!(app.server_form_fields[4], "digest");

        app.cycle_auth_type_next();
        assert_eq!(app.server_form_fields[4], "basic");

        app.cycle_auth_type_next();
        assert_eq!(app.server_form_fields[4], "digestbasic");

        app.cycle_auth_type_next();
        assert_eq!(app.server_form_fields[4], "application-level");

        app.cycle_auth_type_next();
        assert_eq!(app.server_form_fields[4], "digest");
    }

    #[test]
    fn server_form_auth_type_cycles_backward() {
        let mut app = test_app();
        app.open_server_add();

        app.server_form_step = 4;
        assert_eq!(app.server_form_fields[4], "digest");

        app.cycle_auth_type_prev();
        assert_eq!(app.server_form_fields[4], "application-level");

        app.cycle_auth_type_prev();
        assert_eq!(app.server_form_fields[4], "digestbasic");

        app.cycle_auth_type_prev();
        assert_eq!(app.server_form_fields[4], "basic");

        app.cycle_auth_type_prev();
        assert_eq!(app.server_form_fields[4], "digest");
    }

    #[test]
    fn server_add_with_basic_auth_saves_correctly() {
        let mut app = test_app();
        app.open_server_add();
        app.server_form_fields[0] = "prod".to_string();
        app.server_form_fields[1] = "prod.example".to_string();
        app.server_form_fields[2] = "admin".to_string();
        app.server_form_fields[3] = "secret".to_string();
        app.server_form_fields[4] = "basic".to_string();
        app.server_form_fields[5] = "n".to_string();

        app.submit_server_form();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(app.config.servers.len(), 1);
        let server = &app.config.servers[0];
        assert_eq!(server.name, "prod");
        assert_eq!(server.auth_type, AuthType::Basic);
        assert_eq!(server.host, "prod.example");
        assert!(!server.insecure);
    }

    #[test]
    fn server_add_creates_default_app_services_endpoint() {
        let mut app = test_app();
        app.open_server_add();
        app.server_form_fields[0] = "local".to_string();
        app.server_form_fields[1] = "localhost".to_string();
        app.server_form_fields[2] = "admin".to_string();
        app.server_form_fields[3] = "admin".to_string();
        app.server_form_fields[4] = "digest".to_string();
        app.server_form_fields[5] = "n".to_string();

        app.submit_server_form();

        assert_eq!(app.config.servers.len(), 1);
        let server = &app.config.servers[0];
        assert_eq!(server.port, 8000);
        assert!(!server.secure);
        assert_eq!(server.app_servers.len(), 1);
        let app_server = &server.app_servers[0];
        assert_eq!(app_server.name, "App-Services");
        assert_eq!(app_server.port, 8000);
        assert!(!app_server.secure);
        assert_eq!(app_server.content_database.as_deref(), Some("Documents"));
        assert_eq!(app_server.modules_database.as_deref(), Some("Modules"));
        assert_eq!(app.config.active_server.as_deref(), Some("local"));
        assert_eq!(app.config.active_app_server.as_deref(), Some("App-Services"));
        assert_eq!(app.config.active_database.as_deref(), Some("Documents"));
        assert_eq!(
            app.config.active_modules_database.as_deref(),
            Some("Modules")
        );
    }

    #[test]
    fn server_edit_preserves_port_and_app_servers() {
        let mut app = test_app();
        app.config.servers = vec![ServerConfig {
            name: "local".to_string(),
            host: "localhost".to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8010,
            secure: true,
            auth_type: AuthType::Basic,
            insecure: true,
            app_servers: vec![AppServerEndpoint {
                name: "custom".to_string(),
                port: 8010,
                secure: true,
                content_database: Some("Content".to_string()),
                modules_database: Some("Modules".to_string()),
            }],
            uri: None,
        }];
        app.config.active_server = Some("local".to_string());
        app.open_servers_interface();
        app.open_server_edit();

        app.server_form_fields[2] = "newuser".to_string();
        app.submit_server_form();

        let server = &app.config.servers[0];
        assert_eq!(server.port, 8010);
        assert!(server.secure);
        assert_eq!(server.app_servers.len(), 1);
        assert_eq!(server.app_servers[0].name, "custom");
    }

    #[test]
    fn databases_command_opens_servers_interface_with_database_focus() {
        let mut app = test_app();
        app.command_input = ":databases".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(
            app.servers_interface_focus,
            ServersInterfaceFocus::Databases
        );
        assert!(
            app.status_message.contains("No server connected"),
            "database command should now guide users through the Servers interface"
        );
    }

    #[test]
    fn servers_interface_tab_cycles_servers_databases_app_servers() {
        let mut app = test_app();
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.servers_interface_focus,
            ServersInterfaceFocus::Databases
        );

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.servers_interface_focus, ServersInterfaceFocus::AppServers);

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.servers_interface_focus, ServersInterfaceFocus::Servers);
    }

    #[test]
    fn servers_interface_database_enter_sets_active_database() {
        let mut app = test_app();
        app.mode = AppMode::Interface(AppInterface::Servers);
        app.servers_interface_focus = ServersInterfaceFocus::Databases;
        app.database_list = vec!["Documents".to_string(), "Schemas".to_string()];
        app.database_list_state.select(Some(1));

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.config.active_database.as_deref(), Some("Schemas"));
        assert_eq!(app.status_message, "Database: Schemas");
    }

    #[test]
    fn servers_interface_app_server_enter_sets_port_and_databases() {
        let mut app = test_app();
        app.config.servers = vec![test_server("local", "localhost")];
        app.config.active_server = Some("local".to_string());
        app.mode = AppMode::Interface(AppInterface::Servers);
        app.servers_interface_focus = ServersInterfaceFocus::AppServers;
        app.app_server_list = vec![AppServerEndpoint {
            name: "my-app-server".to_string(),
            port: 8010,
            secure: false,
            content_database: Some("Content-DB".to_string()),
            modules_database: Some("Modules-DB".to_string()),
        }];
        app.app_server_list_state.select(Some(0));

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.config.active_app_server.as_deref(), Some("my-app-server"));
        assert_eq!(app.config.active_database.as_deref(), Some("Content-DB"));
        assert_eq!(
            app.config.active_modules_database.as_deref(),
            Some("Modules-DB")
        );
        assert_eq!(app.config.servers[0].port, 8010);
    }

    #[test]
    fn module_clone_root_includes_modules_database_segment() {
        let mut app = test_app();
        app.config.active_server = Some("my-ml".to_string());

        let root = app
            .clone_root_for_active_server_and_modules("my-modules-db")
            .expect("clone root should resolve");
        let text = root.to_string_lossy();

        assert!(text.contains("my-ml/CLONES/my-modules-db"));
    }

    #[test]
    fn module_clone_filter_matches_substrings() {
        let mut app = test_app();
        app.module_clone_all_uris = vec![
            "/foo/bar/one.xqy".to_string(),
            "/baz/two.xqy".to_string(),
            "/foo/qux/three.sjs".to_string(),
        ];
        app.module_clone_filter_input = "foo".to_string();

        app.rebuild_module_clone_filtered_uris();

        assert_eq!(app.module_clone_filtered_uris.len(), 2);
        assert!(
            app.module_clone_filtered_uris
                .iter()
                .all(|uri| uri.contains("foo"))
        );
    }

    #[test]
    fn parse_modules_database_override_reads_single_line_marker() {
        let query = "xquery version \"1.0-ml\";\n(:~modules-database:foo-db:)\n1";
        assert_eq!(
            parse_modules_database_override(query).as_deref(),
            Some("foo-db")
        );
    }

    #[test]
    fn parse_modules_database_override_ignores_inline_text_after_marker() {
        let query = "(:~modules-database:foo-db:) trailing";
        assert_eq!(parse_modules_database_override(query), None);
    }

    #[test]
    fn parse_modules_database_override_returns_none_when_absent() {
        let query = "xquery version \"1.0-ml\";\n1";
        assert_eq!(parse_modules_database_override(query), None);
    }

    #[test]
    fn server_switch_keeps_servers_interface_open() {
        let mut app = test_app();
        app.focus_command_panel();
        app.config.servers = vec![test_server("local", "localhost")];
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(app.config.active_server.as_deref(), Some("local"));
    }

    #[test]
    fn modal_input_query_results_support_vim_page_navigation() {
        let mut app = test_app();
        app.focus_results_panel();
        app.page_size = 10;
        app.set_query_results((0..30).map(|i| format!("result-{i}")).collect());

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(10));

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(0));
    }

    #[test]
    fn modal_input_query_results_support_vim_end_and_top_navigation() {
        let mut app = test_app();
        app.focus_results_panel();
        app.set_query_results((0..5).map(|i| format!("result-{i}")).collect());

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(4));

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        );
        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(0));
    }

    #[test]
    fn status_identity_spans_excludes_collection_and_highlights_connection() {
        let mut app = test_app();
        app.config.servers.push(ServerConfig {
            name: "dockerlocal".to_string(),
            host: "localhost".to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8000,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: vec![],
            uri: None,
        });
        app.config.active_server = Some("dockerlocal".to_string());
        app.config.active_database = Some("Documents".to_string());
        app.current_collection = Some("/test/retention/set/r".to_string());

        let spans = app.status_identity_spans();
        assert_eq!(spans.len(), 6);

        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("/test/retention/set/r"),
            "collection should not appear in identity spans"
        );

        assert_eq!(
            spans[0].style,
            Style::default().fg(Color::Rgb(100, 100, 100)),
            "separator should be dark gray"
        );
        assert_eq!(
            spans[1].style,
            Style::default().fg(Color::Rgb(6, 12, 28)).add_modifier(Modifier::BOLD),
            "server should be highlighted"
        );
        assert_eq!(
            spans[2].style,
            Style::default().fg(Color::Rgb(100, 100, 100)),
            "port separator should be dark gray"
        );
        assert_eq!(
            spans[3].style,
            Style::default()
                .fg(Color::Rgb(6, 12, 28))
                .add_modifier(Modifier::BOLD),
            "port should be highlighted"
        );
        assert_eq!(
            spans[4].style,
            Style::default().fg(Color::Rgb(100, 100, 100)),
            "separator should be dark gray"
        );
        assert_eq!(
            spans[5].style,
            Style::default().fg(Color::Rgb(6, 12, 28)).add_modifier(Modifier::BOLD),
            "database should be highlighted"
        );
    }

    #[test]
    fn status_line_excludes_collection_and_status_message() {
        let mut app = test_app();
        app.config.servers.push(ServerConfig {
            name: "dockerlocal".to_string(),
            host: "localhost".to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8000,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: vec![],
            uri: None,
        });
        app.config.active_server = Some("dockerlocal".to_string());
        app.config.active_database = Some("Documents".to_string());
        app.current_collection = Some("/test/retention/set/r".to_string());
        app.status_message = "Something happened".to_string();

        let line = app.status_line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(text.contains("NORMAL"), "mode should appear");
        assert!(text.contains("dockerlocal:8000"), "server and port should appear");
        assert!(text.contains("Documents"), "database should appear");
        assert!(
            !text.contains("/test/retention/set/r"),
            "collection should not appear"
        );
        assert!(
            !text.contains("Something happened"),
            "status message should not appear"
        );
    }

    #[test]
    fn status_line_stops_at_database() {
        let mut app = test_app();
        app.config.servers.push(ServerConfig {
            name: "srv".to_string(),
            host: "localhost".to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8010,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: vec![],
            uri: None,
        });
        app.config.active_server = Some("srv".to_string());
        app.config.active_database = Some("db".to_string());

        let line = app.status_line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        // Expected: "  NORMAL · srv:8010 · db"
        assert!(text.starts_with("  NORMAL"));
        assert!(text.ends_with("db"));
        assert!(text.contains("srv:8010"), "expected srv:8010 in: {}", text);
        let parts: Vec<&str> = text.split(" · ").collect();
        assert_eq!(
            parts.len(),
            3,
            "should be exactly 3 parts: mode, server, database"
        );
    }

    #[test]
    fn fullscreen_e_triggers_editor_when_document_active() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = Some(crate::client::DocumentDetail {
            uri: "/test/doc.xml".to_string(),
            content: "<root/>".to_string(),
            collections: vec![],
            permissions: vec![],
            quality: None,
        });

        // Ensure $EDITOR is unset so the test does not launch a real editor.
        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .unwrap();

        // Restore $EDITOR in case other tests rely on it.
        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        // open_document_in_external_editor always sets this flag, even on error.
        assert!(
            app.needs_terminal_refresh,
            "external editor should set refresh flag"
        );
    }

    #[test]
    fn fullscreen_e_edits_content_without_active_document() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = None;
        app.full_view_content = "test content".to_string();

        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .unwrap();

        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        assert!(
            app.needs_terminal_refresh,
            "e should open editor even without active_document"
        );
    }

    #[test]
    fn fullscreen_question_opens_help_overlay() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::HelpOverlay);
        assert_eq!(app.previous_mode, Some(AppMode::FullScreenView));
    }

    #[test]
    fn fullscreen_ctrl_e_no_longer_edits() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = Some(crate::client::DocumentDetail {
            uri: "/test/doc.xml".to_string(),
            content: "<root/>".to_string(),
            collections: vec![],
            permissions: vec![],
            quality: None,
        });

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        )
        .unwrap();

        assert!(
            !app.needs_terminal_refresh,
            "Ctrl+E should no longer trigger editor"
        );
    }

    #[test]
    fn query_panel_e_opens_editor_in_navigate_mode() {
        let mut app = test_app();
        app.focus = Focus::Query;
        app.edit_mode = EditMode::Navigate;
        app.query_editor = App::new_query_editor(vec!["test content".to_string()]);

        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        assert!(
            app.needs_terminal_refresh,
            "e in navigate mode should open external editor"
        );
    }

    #[test]
    fn query_panel_e_does_not_open_editor_in_insert_mode() {
        let mut app = test_app();
        app.focus = Focus::Query;
        app.edit_mode = EditMode::Insert;
        app.query_editor = App::new_query_editor(vec!["test content".to_string()]);

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        assert!(
            !app.needs_terminal_refresh,
            "e in insert mode should type letter, not open editor"
        );
        let text: String = app.query_text();
        assert!(text.contains('e'), "e should be inserted in insert mode");
    }

    #[test]
    fn query_results_fullscreen_e_edits_content() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = None;
        app.full_view_content = "query result content".to_string();

        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .unwrap();

        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        assert!(
            app.needs_terminal_refresh,
            "e should open editor for query results fullscreen"
        );
    }

    #[test]
    fn logs_command_opens_log_viewer() {
        let mut app = test_app();
        app.command_input = ":logs".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::LogViewer);
    }

    #[test]
    fn log_viewer_q_restores_previous_mode() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.previous_mode = Some(AppMode::StartPage);
        app.log_view_content = "line1\nline2".to_string();
        app.log_viewer_area_height = 10;
        app.log_view_scroll = 1;

        handle_log_viewer_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.log_view_scroll, 1);
    }
}

fn gzip_file(path: &Path) -> Result<()> {
    let gz_path = PathBuf::from(format!("{}.gz", path.display()));
    let mut input = fs::File::open(path)?;
    let output = fs::File::create(&gz_path)?;
    let mut encoder = GzEncoder::new(output, Compression::default());
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    fs::remove_file(path)?;
    Ok(())
}

fn gzip_logs_older_than_today(log_dir: &Path, today: chrono::NaiveDate) -> Result<()> {
    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if !file_name.starts_with("marklogic-tui-") || !file_name.ends_with(".log") {
            continue;
        }

        let modified = entry.metadata()?.modified()?;
        let modified_date = DateTime::<Local>::from(modified).date_naive();
        if modified_date < today {
            gzip_file(&path)?;
        }
    }
    Ok(())
}

fn rotate_log_on_start(log_path: &Path) -> Result<()> {
    if !log_path.exists() {
        return Ok(());
    }

    let metadata = fs::metadata(log_path)?;
    let modified = metadata.modified().unwrap_or(SystemTime::now());
    let modified_local: DateTime<Local> = DateTime::from(modified);
    let timestamp = modified_local.format("%Y-%m-%d_%H-%M-%S");
    let rotated = log_path.with_file_name(format!(
        "marklogic-tui-{}-{}.log",
        timestamp,
        std::process::id()
    ));

    fs::rename(log_path, &rotated)?;

    let today = Local::now().date_naive();
    if modified_local.date_naive() < today {
        gzip_file(&rotated)?;
    }

    Ok(())
}

fn init_file_logging() -> Result<()> {
    let log_path = PathBuf::from(LOG_FILE_PATH);
    let log_dir = log_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid log path: {}", log_path.display()))?;

    rotate_log_on_start(&log_path)?;
    gzip_logs_older_than_today(log_dir, Local::now().date_naive())?;

    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || {
            log_file
                .try_clone()
                .expect("failed to clone log file handle")
        })
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;
    debug!(path = %log_path.display(), "logging initialized");

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let use_edtui = args.inline_editor.as_deref() == Some("edtui");

    init_file_logging()?;
    info!(use_edtui, "starting marklogic-tui");

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(use_edtui)?;

    loop {
        if app.needs_terminal_refresh {
            terminal.clear()?;
            app.needs_terminal_refresh = false;
        }
        terminal.draw(|f| ui::ui(f, &mut app))?;
        if events::handle_event(&mut app)? {
            break;
        }
        app.maybe_autosave();
        app.clear_expired_status_message();
        app.log_status_message_if_changed();
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    info!("shutting down marklogic-tui");
    Ok(())
}
