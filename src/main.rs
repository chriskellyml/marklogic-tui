mod client;
mod config;
mod events;
mod external_editor;
mod help;
mod keyboard;
mod logs;
mod logs_ui;
mod modules_cache;
mod query_error;
mod query_file;
mod query_highlight;
mod query_result_cache;
mod reports;
mod reports_ui;
mod system;
mod tracked_folder;
mod ui;
mod xquery_imports;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use clap::Parser;
use client::{AppServerEndpoint, AuthType, MarkLogicClient, SearchResult, ServerConfig};
use config::{AppConfig, CONFIG_BUNDLE_VERSION, ConfigBundle};
use crossterm::{
    event::{
        self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use edtui::{
    EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, LineNumbers, Lines,
};
#[cfg(test)]
use events::{
    handle_command_key, handle_filter_key, handle_fullscreen_key, handle_key_event,
    handle_log_viewer_key, handle_multi_run_form_key, handle_navigation_mode_key,
    handle_query_insert_transition_key, handle_query_key, handle_results_key,
    handle_return_to_start_page_key, handle_server_delete_confirm_key,
    handle_servers_interface_key, map_query_navigation_key,
};
use external_editor::temp_editor_path;
use flate2::{Compression, write::GzEncoder};
use query_file::{
    QueryExecutionKind, QueryTree, QueryTreeRow, build_query_tree_rows, default_query_location,
    default_query_root, discover_query_tree, display_query_path, editor_lines,
    is_default_query_root, is_supported_query_file, load_query_file, parse_relative_query_path,
    prune_empty_query_folders, query_execution_kind, remap_path_prefix,
    require_folder_for_query_file, save_query_file, should_autosave,
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
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Widget, Wrap,
    },
};
use ratatui_textarea::{Input, TextArea};
use std::str::FromStr;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, info, warn};
use tracked_folder::{TrackedFolderEntry, TrackedFolderStore, canonicalize_folder};
use ui::{display_folder_path, format_document_detail, resolve_folder_input};

const LOG_FILE_PATH: &str = "/tmp/marklogic-tui.log";

/// Shown in the results pane when a query finishes without any output.
const NO_OUTPUT_MESSAGE: &str = "No output";

/// Upper bound for multi-run repetitions, guarding against accidental huge runs.
const MAX_MULTI_RUN_COUNT: u32 = 1000;

#[derive(Parser, Debug)]
struct Args {
    /// Override the saved inline editor: "edtui" (Vim modes and highlighting) or "textarea" (simpler).
    #[arg(long, value_name = "EDITOR")]
    inline_editor: Option<String>,
    /// Export all configuration (servers, tracked folders) to a YAML file and exit. Experimental.
    #[arg(long, value_name = "FILE", conflicts_with = "config")]
    export_config: Option<PathBuf>,
    /// Import configuration from a YAML file before starting, replacing the on-disk config. Experimental.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
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

/// The presentation applied to values returned by a query.  The raw results
/// remain untouched so that changing presentation is always reversible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryResultsFormat {
    Plain,
    Csv,
    Psv,
    Tsv,
    Uris,
    Json,
}

impl QueryResultsFormat {
    const ALL: [Self; 6] = [
        Self::Plain,
        Self::Csv,
        Self::Psv,
        Self::Tsv,
        Self::Uris,
        Self::Json,
    ];

    fn picker_index(self) -> usize {
        Self::ALL
            .iter()
            .position(|format| *format == self)
            .expect("all query result formats are listed")
    }

    fn label(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Csv => "csv",
            Self::Psv => "psv",
            Self::Tsv => "tsv",
            Self::Uris => "uris",
            Self::Json => "json",
        }
    }
}

#[derive(Debug, Clone)]
struct UriQueryResult {
    uri: String,
    collections: Vec<String>,
    permissions: Vec<String>,
    property_count: Option<usize>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Screen {
    Home,
    Query,
    Documents,
    Logs,
    Reports,
    Servers,
    System,
    DocumentView,
}

#[derive(Clone)]
struct ScreenSnapshot {
    mode: AppMode,
    focus: Focus,
    edit_mode: EditMode,
    editor_mode: EditorMode,
    query_visible: bool,
    previous_mode: Option<AppMode>,
    modal_origin_focus: Option<Focus>,
    interface_origin_focus: Option<Focus>,
    command_input: String,
    server_picker_open: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum AppMode {
    StartPage,
    Normal,
    FullScreenView,
    System,
    MarkLogicLogs,
    Reports,
    Interface(AppInterface),
    ServerForm,
    ServerDeleteConfirm,
    AppServerForm,
    AppServerDeleteConfirm,
    AppServerDiscovery,
    CollectionSelect,
    DeleteConfirm,
    QueryFileSelect,
    QueryFileCreate,
    QueryFileRename,
    QueryFileDeleteConfirm,
    QueryResultFormatSelect,
    ActiveVariables,
    DeclaredVariables,
    MultiRunForm,
    ModuleCloneSelect,
    ModuleCreate,
    ModuleResetConfirm,
    ModuleDropConfirm,
    ModuleDiffView,
    ModuleDropCacheConfirm,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseTarget {
    SystemLogs,
    SystemSettings,
    SystemSetting(usize),
    Query,
    Results,
    Command,
    ServerFavorites,
    ServerTree,
}

#[derive(Debug, Clone, PartialEq)]
enum ServerFormMode {
    Add,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServersInterfaceFocus {
    Favorites,
    Tree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServersInterfaceRow {
    Server {
        server_name: String,
    },
    Group {
        server_name: String,
        group_name: String,
    },
    AppServer {
        server_name: String,
        group_name: String,
        app_server_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackedFolderMenuItem {
    Header(&'static str),
    Folder(TrackedFolderEntry),
}

struct ModuleSearchState {
    pattern: String,
    results: Vec<String>,
    selected: ListState,
    page: usize,
    page_size: usize,
    total: Option<usize>,
    focus_input: bool,
    /// URIs marked for download, in mark order. Survives new searches and
    /// paging within one picker visit; cleared when the picker opens.
    marked: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncInfo {
    db: String,
    uri: String,
}

/// A query currently executing in the background, keyed by query file path.
#[derive(Debug)]
struct RunningQuery {
    started_at: Instant,
    abort: tokio::task::AbortHandle,
}

/// Completion message sent from a background query task to the UI thread.
#[derive(Debug)]
struct QueryCompletion {
    path: PathBuf,
    started_at: Instant,
    outcome: Result<Vec<String>, String>,
    /// Total runs for a multi-run, or 1 for a single run.
    total_runs: usize,
}

/// Progress report sent from a background multi-run task to the UI thread,
/// carrying the cumulative results assembled so far (newest run first).
#[derive(Debug)]
struct QueryProgress {
    path: PathBuf,
    run: usize,
    total: usize,
    cumulative: Vec<String>,
}

/// Events sent from background query tasks to the UI thread.
#[derive(Debug)]
enum QueryEvent {
    Progress(QueryProgress),
    Completion(QueryCompletion),
}

struct App {
    config: AppConfig,
    client: Option<MarkLogicClient>,
    focus: Focus,
    terminal_focused: bool,
    edit_mode: EditMode,
    mode: AppMode,
    previous_mode: Option<AppMode>,
    previous_screen: Option<Screen>,
    screen_states: HashMap<Screen, ScreenSnapshot>,
    help_scroll: u16,
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
    query_folders: Vec<PathBuf>,
    /// Supported files sitting directly in the query root. Not shown in the tree.
    loose_query_files: Vec<PathBuf>,
    /// Folders the user has collapsed in the Files tree. Folders are expanded
    /// unless listed here.
    collapsed_query_folders: HashSet<PathBuf>,
    active_query_file: Option<PathBuf>,
    /// Selection index into the visible rows of the Files tree.
    query_file_list_state: ListState,
    module_search: ModuleSearchState,
    module_drop_needs_reset: bool,
    modules_view: bool,
    cached_modules: Vec<modules_cache::CachedModuleStatus>,
    /// Selection index into the visible rows of the Modules tree.
    cached_modules_state: ListState,
    /// Collapsed folders of the Modules tree (absolute working-copy paths).
    collapsed_module_folders: HashSet<PathBuf>,
    module_diff_uri: Option<String>,
    module_diff_lines: Vec<modules_cache::DiffLine>,
    module_diff_scroll: u16,
    /// Pending drop-cache confirm: (uri, dirty, unsynced) at request time.
    module_drop_target: Option<(String, bool, bool)>,
    new_module_uri_input: String,
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
    // Multi-run form (count + wait seconds) and last-used values for prefill.
    multi_run_form_fields: Vec<String>,
    multi_run_form_step: usize,
    last_multi_run_count: u32,
    last_multi_run_wait_secs: f64,
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
    query_results_format: QueryResultsFormat,
    query_results_format_picker_state: ListState,
    /// Wildcard filter applied only to the currently displayed query results.
    query_results_filter: Option<String>,
    uri_query_results: Vec<UriQueryResult>,
    query_uri_delete_target: Option<String>,
    // Background query execution: running tasks keyed by query file path,
    // plus the channel they report completion on. Partial multi-run results
    // streamed so far, keyed the same way.
    running_queries: HashMap<PathBuf, RunningQuery>,
    running_partial_results: HashMap<PathBuf, Vec<String>>,
    query_tx: UnboundedSender<QueryEvent>,
    query_rx: UnboundedReceiver<QueryEvent>,
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
    mouse_regions: Vec<(Rect, MouseTarget)>,
    environment_area: Rect,
    server_picker_open: bool,
    server_picker_error: Option<String>,
    server_picker_area: Rect,
    server_picker_state: ListState,
    last_query_results_g: Option<Instant>,
    // Full screen view
    active_document: Option<client::DocumentDetail>,
    full_view_content: String,
    full_view_scroll: u16,
    last_fullscreen_g: Option<Instant>,
    fullscreen_area_height: u16,
    server_logs: logs::LogViewer,
    reports: reports_ui::Reports,
    system: system::System,
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
    servers_interface_favorites: Vec<ServersInterfaceRow>,
    servers_interface_tree: Vec<ServersInterfaceRow>,
    servers_interface_favorites_state: ListState,
    servers_interface_tree_state: TableState,
    expanded_servers: HashSet<String>,
    // App server management
    app_server_form_mode: ServerFormMode,
    app_server_form_step: usize,
    app_server_form_fields: Vec<String>,
    app_server_edit_target: Option<String>,
    app_server_edit_group: Option<String>,
    app_server_delete_target: Option<String>,
    app_server_delete_group: Option<String>,
    app_server_list_server_name: Option<String>,
    app_server_discovery_parent: Option<String>,
    app_server_discovery_results: Vec<AppServerEndpoint>,
    app_server_discovery_state: TableState,
    app_server_discovery_marked: HashSet<String>,
    // Document form (create / metadata edit)
    doc_form_step: usize,
    doc_form_uri: String,
    doc_form_collections: String,
    doc_form_quality: String,
    doc_form_content: String,
    active_variable_fields: Vec<(String, String)>,
    active_variable_step: usize,
    declared_variable_fields: Vec<(String, String)>,
    declared_variable_step: usize,
    config_bundle_path: Option<PathBuf>,
    // Autocomplete
    autocomplete_suggestions: Vec<&'static str>,
    autocomplete_selected: usize,
    // App server selection
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
    fn screen_for_mode(mode: &AppMode, query_visible: bool, previous: Option<&AppMode>) -> Screen {
        match mode {
            AppMode::StartPage => Screen::Home,
            AppMode::Normal => {
                if query_visible {
                    Screen::Query
                } else {
                    Screen::Documents
                }
            }
            AppMode::MarkLogicLogs => Screen::Logs,
            AppMode::Reports => Screen::Reports,
            AppMode::System => Screen::System,
            AppMode::FullScreenView => Screen::DocumentView,
            AppMode::Interface(_)
            | AppMode::ServerForm
            | AppMode::ServerDeleteConfirm
            | AppMode::AppServerForm
            | AppMode::AppServerDeleteConfirm
            | AppMode::AppServerDiscovery => Screen::Servers,
            AppMode::HelpOverlay => previous
                .filter(|mode| **mode != AppMode::HelpOverlay)
                .map_or(
                    if query_visible {
                        Screen::Query
                    } else {
                        Screen::Documents
                    },
                    |mode| Self::screen_for_mode(mode, query_visible, None),
                ),
            AppMode::CollectionSelect
            | AppMode::DeleteConfirm
            | AppMode::DocumentCreate
            | AppMode::DocumentMetadataEdit => {
                if query_visible {
                    Screen::Query
                } else {
                    Screen::Documents
                }
            }
            _ => Screen::Query,
        }
    }

    fn current_screen(&self) -> Screen {
        Self::screen_for_mode(&self.mode, self.query_visible, self.previous_mode.as_ref())
    }

    fn capture_screen(&self) -> ScreenSnapshot {
        ScreenSnapshot {
            mode: self.mode.clone(),
            focus: self.focus.clone(),
            edit_mode: self.edit_mode.clone(),
            editor_mode: self.edtui_state.mode,
            query_visible: self.query_visible,
            previous_mode: self.previous_mode.clone(),
            modal_origin_focus: self.modal_origin_focus.clone(),
            interface_origin_focus: self.interface_origin_focus.clone(),
            command_input: self.command_input.clone(),
            server_picker_open: self.server_picker_open,
        }
    }

    fn restore_screen(&mut self, state: ScreenSnapshot) {
        self.mode = state.mode;
        self.focus = state.focus;
        self.edit_mode = state.edit_mode;
        self.edtui_state.mode = state.editor_mode;
        self.query_visible = state.query_visible;
        self.previous_mode = state.previous_mode;
        self.modal_origin_focus = state.modal_origin_focus;
        self.interface_origin_focus = state.interface_origin_focus;
        self.command_input = state.command_input;
        self.server_picker_open = state.server_picker_open;
        self.autocomplete_suggestions.clear();
        self.autocomplete_selected = 0;
        if self.focus == Focus::Command && !self.command_input.is_empty() {
            self.update_autocomplete();
        }
    }

    fn switch_screen(&mut self, target: Screen) {
        if self.current_screen() == target {
            return;
        }
        let origin = self.mode.clone();
        if target == Screen::System {
            self.system.return_mode = Some(origin.clone());
            self.system.return_screen = Some(self.current_screen());
        }
        if let Some(state) = self.screen_states.get(&target).cloned() {
            self.restore_screen(state);
            if target == Screen::Logs {
                self.server_logs
                    .resume(self.client.clone(), origin, &self.rt);
            }
            if target == Screen::Reports {
                self.reconcile_report_source();
            }
            return;
        }
        self.server_picker_open = false;
        match target {
            Screen::Home => self.enter_start_page(),
            Screen::Query => {
                self.mode = AppMode::Normal;
                self.show_query_editor();
            }
            Screen::Documents => {
                self.mode = AppMode::Normal;
                self.query_visible = false;
                self.focus_results_panel();
            }
            Screen::Logs => {
                self.server_logs
                    .resume(self.client.clone(), origin, &self.rt);
                self.mode = AppMode::MarkLogicLogs;
            }
            Screen::Reports => self.open_reports(""),
            Screen::Servers => self.cmd_servers(),
            Screen::System => self.open_system(),
            Screen::DocumentView => {}
        }
    }

    fn export_config_bundle(&self) -> Result<()> {
        let Some(path) = self.config_bundle_path.as_ref() else {
            return Ok(());
        };

        let bundle = ConfigBundle {
            version: CONFIG_BUNDLE_VERSION,
            config: self.config.clone(),
            tracked_folders: self.tracked_folders.clone(),
        };
        bundle.export_to(path)
    }

    fn save_app_config(&self) -> Result<()> {
        self.config.save()?;
        self.export_config_bundle()
            .context("Config saved locally but failed to update the --config YAML file")
    }

    fn save_tracked_folders(&self) -> Result<()> {
        self.tracked_folders.save()?;
        self.export_config_bundle()
            .context("Folders saved locally but failed to update the --config YAML file")
    }

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

    fn new(use_edtui: bool, config_bundle_path: Option<PathBuf>) -> Result<Self> {
        let config = AppConfig::load()?;
        let autosave_interval = Duration::from_secs(config.system.autosave_seconds);
        let rt = Runtime::new()?;
        let (query_tx, query_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tracked_folders = TrackedFolderStore::load()?;
        // Start in the default query store. The launch directory is recorded as
        // a tracked folder so it can be opened later with `:folders`.
        let launch_dir =
            canonicalize_folder(&env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))?;
        tracked_folders.record_access(&launch_dir)?;
        tracked_folders.save()?;
        let query_root_dir = default_query_root()?;
        let query_result_cache_dir = query_root_dir.join(".marklogic-tui");
        let tree = load_query_tree(&query_root_dir, &query_result_cache_dir)?;
        let active_query_file = tree.files.first().cloned();
        let loose_query_files = tree.loose_files.clone();
        let client = Self::build_client_from_config(&config);

        let mut app_result = Ok(Self {
            config,
            client,
            focus: Focus::Command,
            terminal_focused: true,
            edit_mode: EditMode::Navigate,
            mode: AppMode::StartPage,
            previous_mode: None,
            previous_screen: None,
            screen_states: HashMap::new(),
            help_scroll: 0,
            modal_origin_focus: None,
            interface_origin_focus: None,
            command_input: String::new(),
            query_editor: Self::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            should_quit: false,
            query_root_dir,
            query_result_cache_dir,
            query_files: tree.files,
            query_folders: tree.folders,
            loose_query_files,
            collapsed_query_folders: HashSet::new(),
            active_query_file,
            query_file_list_state: ListState::default(),
            module_search: ModuleSearchState {
                pattern: String::new(),
                results: Vec::new(),
                selected: ListState::default(),
                page: 0,
                page_size: 50,
                total: None,
                focus_input: true,
                marked: Vec::new(),
            },
            module_drop_needs_reset: false,
            modules_view: false,
            cached_modules: Vec::new(),
            cached_modules_state: ListState::default(),
            collapsed_module_folders: HashSet::new(),
            module_diff_uri: None,
            module_diff_lines: Vec::new(),
            module_diff_scroll: 0,
            module_drop_target: None,
            new_module_uri_input: String::new(),
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
            multi_run_form_fields: vec![String::new(); 2],
            multi_run_form_step: 0,
            last_multi_run_count: 5,
            last_multi_run_wait_secs: 0.0,
            query_dirty: false,
            query_last_edit: None,
            query_autosave_interval: autosave_interval,
            results_text: String::new(),
            status_message: String::new(),
            transient_status_message: None,
            transient_status_expires_at: None,
            query_results: Vec::new(),
            query_results_timestamp: None,
            query_results_state: TableState::default(),
            query_results_format: QueryResultsFormat::Plain,
            query_results_format_picker_state: ListState::default(),
            query_results_filter: None,
            uri_query_results: Vec::new(),
            query_uri_delete_target: None,
            running_queries: HashMap::new(),
            running_partial_results: HashMap::new(),
            query_tx,
            query_rx,
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
            mouse_regions: Vec::new(),
            environment_area: ratatui::layout::Rect::default(),
            server_picker_open: false,
            server_picker_error: None,
            server_picker_area: ratatui::layout::Rect::default(),
            server_picker_state: ListState::default(),
            last_query_results_g: None,
            active_document: None,
            full_view_content: String::new(),
            full_view_scroll: 0,
            last_fullscreen_g: None,
            fullscreen_area_height: 0,
            server_logs: crate::logs::LogViewer::default(),
            reports: crate::reports_ui::Reports::default(),
            system: crate::system::System::default(),
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
            servers_interface_focus: ServersInterfaceFocus::Tree,
            servers_interface_favorites: Vec::new(),
            servers_interface_tree: Vec::new(),
            servers_interface_favorites_state: ListState::default(),
            servers_interface_tree_state: TableState::default(),
            expanded_servers: HashSet::new(),
            app_server_form_mode: ServerFormMode::Add,
            app_server_form_step: 0,
            app_server_form_fields: vec![String::new(); 5], // name, port, secure, content_db, modules_db
            app_server_edit_target: None,
            app_server_edit_group: None,
            app_server_delete_target: None,
            app_server_delete_group: None,
            app_server_list_server_name: None,
            app_server_discovery_parent: None,
            app_server_discovery_results: Vec::new(),
            app_server_discovery_state: TableState::default(),
            app_server_discovery_marked: HashSet::new(),
            doc_form_step: 0,
            doc_form_uri: String::new(),
            doc_form_collections: String::new(),
            doc_form_quality: "0".to_string(),
            doc_form_content: String::new(),
            active_variable_fields: Vec::new(),
            active_variable_step: 0,
            declared_variable_fields: Vec::new(),
            declared_variable_step: 0,
            config_bundle_path,
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
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
            if !app.loose_query_files.is_empty() {
                app.status_message = format!(
                    "{} query file(s) at the top level are not shown. Move them into a folder, e.g. {}",
                    app.loose_query_files.len(),
                    default_query_location().display()
                );
            }
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
        (":logs", "Browse and search cached MarkLogic logs by host"),
        (":reset", "Reset log time cursor and truncation to now"),
        (":reports", "Run reports or edit their YAML definitions"),
        (":system", "TUI diagnostics and system settings"),
        (
            ":app-logs",
            "Open System with the application diagnostic log",
        ),
        (":tdes", "List Template Driven Extraction templates"),
        (":quit", "Quit the application"),
    ];

    fn new_query_editor(lines: Vec<String>) -> TextArea<'static> {
        let mut ta = TextArea::new(if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        });
        ta.set_block(Block::default().borders(Borders::ALL).title(
            "Query [Alt+Enter/F5 run, c clone, e edit, p preview, Ctrl+S save, Ctrl+O switch]",
        ));
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
        let tree = load_query_tree(&self.query_root_dir, &self.query_result_cache_dir)?;
        self.query_files = tree.files;
        self.query_folders = tree.folders;
        self.loose_query_files = tree.loose_files;
        let folders = &self.query_folders;
        self.collapsed_query_folders
            .retain(|folder| folders.contains(folder));
        self.sync_query_selection_to_active();
        Ok(())
    }

    /// Visible rows of the Files tree for the current state.
    fn query_file_rows(&self) -> Vec<QueryTreeRow> {
        build_query_tree_rows(
            &self.query_root_dir,
            &self.query_files,
            &self.query_folders,
            &self.collapsed_query_folders,
        )
    }

    fn selected_query_row(&self) -> Option<QueryTreeRow> {
        let sel = self.query_file_list_state.selected()?;
        self.query_file_rows().into_iter().nth(sel)
    }

    /// Put the selection on the active file, or the first row if the active
    /// file is not visible. Clears the selection when the tree is empty.
    fn sync_query_selection_to_active(&mut self) {
        let rows = self.query_file_rows();
        if rows.is_empty() {
            self.query_file_list_state.select(None);
            return;
        }
        let selected = self
            .active_query_file
            .as_ref()
            .and_then(|active| rows.iter().position(|row| &row.path == active))
            .unwrap_or(0);
        self.query_file_list_state.select(Some(selected));
    }

    /// Select a row without loading anything (used by pickers).
    fn set_query_row_index(&mut self, index: usize) {
        self.query_file_list_state.select(Some(index));
    }

    /// Select a row. If it is a file, load it into the editor.
    fn open_query_row(&mut self, index: usize) {
        let rows = self.query_file_rows();
        let Some(row) = rows.get(index) else {
            return;
        };
        let path = row.path.clone();
        let is_dir = row.is_dir;
        self.set_query_row_index(index);
        if !is_dir {
            self.select_query_file(path);
        }
    }

    /// Expand or collapse a folder and keep the selection on it.
    fn toggle_query_folder(&mut self, path: &Path) {
        if !self.collapsed_query_folders.remove(path) {
            self.collapsed_query_folders.insert(path.to_path_buf());
        }
        self.select_query_path(path);
    }

    /// Select the row for `path` if it is visible.
    fn select_query_path(&mut self, path: &Path) {
        let index = self
            .query_file_rows()
            .iter()
            .position(|row| row.path == path);
        match index {
            Some(index) => self.set_query_row_index(index),
            None => self.sync_query_selection_to_active(),
        }
    }

    fn delete_query_folder(&mut self, path: PathBuf) {
        let is_empty = fs::read_dir(&path)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !is_empty {
            self.status_message = format!(
                "Folder '{}' is not empty. Delete or move its files first.",
                display_query_path(&path, &self.query_root_dir)
            );
            return;
        }
        if let Err(e) = fs::remove_dir(&path) {
            self.status_message = format!("Failed to delete folder: {}", e);
            return;
        }
        let _ =
            remove_query_result_cache(&self.query_result_cache_dir, &self.query_root_dir, &path);
        self.status_message = format!(
            "Deleted empty folder: {}",
            display_query_path(&path, &self.query_root_dir)
        );
        if let Err(e) = self.refresh_query_files() {
            self.status_message = format!("Deleted folder but failed to refresh list: {}", e);
        }
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
            self.save_tracked_folders()?;
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
        self.save_tracked_folders()?;

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

        match self.save_tracked_folders() {
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

        match self.save_tracked_folders() {
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
            (AppMode::System, _) => "SYSTEM",
            (AppMode::MarkLogicLogs, _) => "MARKLOGIC LOGS",
            (AppMode::Reports, _) => "REPORTS",
            _ => self.edit_mode_label(),
        }
    }

    fn edit_mode_style(&self) -> Style {
        let text = Color::Rgb(6, 12, 28);
        match self.edit_mode {
            EditMode::Navigate => Style::default()
                .fg(text)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
            EditMode::Insert => Style::default()
                .fg(text)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        }
    }

    fn edit_mode_accent_color(&self) -> Color {
        match self.edit_mode {
            EditMode::Navigate => Color::Cyan,
            EditMode::Insert => Color::Red,
        }
    }

    fn focus_command_panel(&mut self) {
        self.focus = Focus::Command;
        self.set_edit_mode(EditMode::Navigate);
    }

    fn apply_focus(&mut self, focus: Focus) {
        match focus {
            Focus::Command => self.focus_command_panel(),
            Focus::Query => self.focus_query_panel(),
            Focus::Results => self.focus_results_panel(),
            Focus::Filter => {
                self.focus = Focus::Filter;
                self.set_edit_mode(EditMode::Navigate);
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
                | AppMode::QueryResultFormatSelect
                | AppMode::ActiveVariables
                | AppMode::DeclaredVariables
                | AppMode::MultiRunForm
                | AppMode::ModuleCloneSelect
                | AppMode::ModuleCreate
                | AppMode::ModuleResetConfirm
                | AppMode::ModuleDropConfirm
                | AppMode::ModuleDiffView
                | AppMode::ModuleDropCacheConfirm
                | AppMode::TrackedFolderSelect
                | AppMode::TrackedFolderAdd
                | AppMode::TrackedFolderDeleteConfirm
                | AppMode::TrackedFolderCacheClearConfirm
        )
    }

    /// True when `?` should be typed into a focused text field instead of
    /// opening the help overlay.
    fn help_key_types_text(&self) -> bool {
        match self.mode {
            AppMode::ServerForm
            | AppMode::AppServerForm
            | AppMode::QueryFileCreate
            | AppMode::QueryFileRename
            | AppMode::DeclaredVariables
            | AppMode::MultiRunForm
            | AppMode::TrackedFolderAdd
            | AppMode::DocumentCreate
            | AppMode::DocumentMetadataEdit => true,
            AppMode::System => self.system.search_editing,
            AppMode::Reports => self.reports.form.is_some(),
            AppMode::MarkLogicLogs => {
                self.server_logs.search_editing
                    || self.server_logs.truncation_dialog.is_some()
                    || self.server_logs.time_editing
                    || self.server_logs.file_filter_editing
            }
            AppMode::ModuleCloneSelect => self.module_search.focus_input,
            AppMode::Normal => self.focus == Focus::Query && self.edit_mode == EditMode::Insert,
            _ => false,
        }
    }

    fn open_help_overlay(&mut self) {
        self.previous_mode = Some(self.mode.clone());
        self.help_scroll = 0;
        self.mode = AppMode::HelpOverlay;
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
        self.set_edit_mode(EditMode::Navigate);
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
        self.server_picker_open = false;
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
        self.set_edit_mode(EditMode::Navigate);
        if self.query_file_list_state.selected().is_none() {
            self.sync_query_selection_to_active();
        }
    }

    fn focus_results_panel(&mut self) {
        self.focus = Focus::Results;
        self.set_edit_mode(EditMode::Navigate);
    }

    fn toggle_file_list(&mut self) {
        self.file_list_visible = !self.file_list_visible;
    }

    /// Handle a key when the modules sidebox is active (modules_view == true).
    /// Returns true when the key was consumed.
    fn handle_modules_sidebox_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        if !self.modules_view {
            return false;
        }
        // Uppercase T toggles back to files in both branches.
        if matches!(key.code, KeyCode::Char('T')) {
            self.toggle_modules_view();
            return true;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cached_module_selection(-1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cached_module_selection(1);
                true
            }
            KeyCode::Enter => match self.selected_module_row() {
                Some(row) if row.is_dir => {
                    self.toggle_module_folder(&row.path);
                    true
                }
                Some(row) => {
                    if let Some(status) = self.module_status_for_working(&row.path) {
                        self.load_cached_module_into_editor(&status.uri);
                    } else {
                        self.status_message = "No cached module selected.".to_string();
                    }
                    true
                }
                None => {
                    self.status_message = "No cached module selected.".to_string();
                    true
                }
            },
            KeyCode::Char('e') if key.modifiers.is_empty() => {
                self.open_cached_module_in_external_editor();
                true
            }
            KeyCode::Char('d') if key.modifiers.is_empty() => {
                self.show_cached_module_diff();
                true
            }
            KeyCode::Char('s') if key.modifiers.is_empty() => {
                self.sync_all_cached_modules();
                true
            }
            KeyCode::Char('r') if key.modifiers.is_empty() => {
                self.reset_selected_to_original();
                true
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                self.open_module_create();
                true
            }
            KeyCode::Char('D') => {
                self.request_drop_cached_selected();
                true
            }
            KeyCode::Char('M') | KeyCode::Delete => {
                self.status_message =
                    "Not available for cached modules — T for Files view.".to_string();
                true
            }
            KeyCode::Char('m') if key.modifiers.is_empty() => {
                self.open_module_picker();
                true
            }
            KeyCode::Char('G') if key.modifiers.is_empty() => {
                let row_count = self.module_tree_rows().len();
                if row_count > 0 {
                    self.open_module_row(row_count - 1);
                }
                true
            }
            KeyCode::Char('g') if key.modifiers.is_empty() => {
                if !self.module_tree_rows().is_empty() {
                    self.open_module_row(0);
                }
                true
            }
            _ if key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                false
            }
            _ => false,
        }
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
        self.filter_input = if self.filtering_query_results() {
            self.query_results_filter.clone().unwrap_or_default()
        } else {
            self.uri_filter.clone().unwrap_or_default()
        };
    }

    fn filtering_query_results(&self) -> bool {
        !self.query_results.is_empty() && self.records.is_empty()
    }

    fn visible_query_result_indices(&self) -> Vec<usize> {
        match self.query_results_filter.as_deref() {
            Some(pattern) if !pattern.is_empty() => self
                .query_results
                .iter()
                .enumerate()
                .filter_map(|(index, result)| wildcard_matches(pattern, result).then_some(index))
                .collect(),
            _ => (0..self.query_results.len()).collect(),
        }
    }

    fn visible_query_results(&self) -> Vec<&str> {
        self.visible_query_result_indices()
            .into_iter()
            .filter_map(|index| self.query_results.get(index).map(String::as_str))
            .collect()
    }

    fn visible_query_result_count(&self) -> usize {
        self.visible_query_result_indices().len()
    }

    fn apply_query_results_filter(&mut self, pattern: String) {
        self.query_results_filter = (!pattern.is_empty()).then_some(pattern);
        let visible = self.visible_query_result_count();
        self.query_results_state.select((visible > 0).then(|| {
            self.query_results_state
                .selected()
                .unwrap_or(0)
                .min(visible - 1)
        }));
        self.last_query_results_g = None;
        if self.query_results_format == QueryResultsFormat::Uris {
            self.load_uri_query_results();
        }
    }

    /// Set navigate/insert mode. When edtui is the editor, its own mode follows
    /// so that typing and the app's navigation keys agree.
    fn set_edit_mode(&mut self, mode: EditMode) {
        if self.use_edtui {
            self.edtui_state.mode = match mode {
                EditMode::Navigate => EditorMode::Normal,
                EditMode::Insert => EditorMode::Insert,
            };
        }
        self.edit_mode = mode;
    }

    /// Mirror edtui's mode (after `i`, `a`, `v`, Esc, ...) into `edit_mode`.
    /// Any non-Normal edtui mode counts as insert, so the app's navigation
    /// keys stay out of the way while text is being edited.
    fn sync_edit_mode_from_editor(&mut self) {
        if self.use_edtui {
            self.edit_mode = match self.edtui_state.mode {
                EditorMode::Normal => EditMode::Navigate,
                _ => EditMode::Insert,
            };
        }
    }

    fn enter_insert_mode(&mut self) {
        if self.focus == Focus::Query {
            self.set_edit_mode(EditMode::Insert);
        } else {
            self.status_message = "Insert mode only applies to the query editor.".to_string();
        }
    }

    fn exit_insert_mode(&mut self) {
        self.set_edit_mode(EditMode::Navigate);
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

    /// Open the compiled form of the active query in the external editor:
    /// XQuery `import` directives expanded and active variables (`%%name%%`)
    /// substituted, exactly as the text would be sent to the server. The
    /// preview is read-only; edits made in the external editor are discarded.
    fn open_compiled_query_preview(&mut self) {
        let query = self.query_text();
        let path = self.active_query_file.clone();
        let kind = path
            .as_deref()
            .map(query_execution_kind)
            .unwrap_or(QueryExecutionKind::Unsupported);
        let compiled = if matches!(kind, QueryExecutionKind::XQuery) {
            match path.as_ref() {
                Some(path) => match xquery_imports::expand_xquery_imports(&query, path) {
                    Ok(expanded) => expanded,
                    Err(e) => {
                        self.status_message = format!("Preview failed: {:#}", e);
                        return;
                    }
                },
                None => query,
            }
        } else {
            query
        };
        let compiled = substitute_active_variables(&compiled, &self.config.active_variables);

        let temp_path = temp_editor_path(path.as_deref());
        let preview_result = fs::write(&temp_path, &compiled)
            .map_err(|e| anyhow::anyhow!("failed to write preview file: {}", e))
            .and_then(|_| external_editor::open_file_in_external_editor(&temp_path));
        let _ = fs::remove_file(&temp_path);
        self.needs_terminal_refresh = true;

        match preview_result {
            Ok(()) => {
                self.status_message = match path.as_ref() {
                    Some(path) => format!(
                        "Previewed compiled query for {}",
                        display_query_path(path, &self.query_root_dir)
                    ),
                    None => "Previewed compiled query.".to_string(),
                };
            }
            Err(e) => {
                error!(error = %e, "compiled query preview failed");
                self.status_message = format!("Preview failed: {}", e);
            }
        }
    }

    fn refresh_log_viewer_content(&mut self) {
        if self.system.refresh_log(&self.log_file_path) {
            self.log_view_content = self.system.filtered_log();
        }
    }

    fn log_viewer_max_scroll(&self) -> u16 {
        let total_lines = self.log_view_content.lines().count().min(u16::MAX as usize) as u16;
        let visible_lines = self.log_viewer_area_height.saturating_sub(2);
        total_lines.saturating_sub(visible_lines)
    }

    fn open_system(&mut self) {
        if self.current_screen() != Screen::System {
            self.system.return_mode = Some(self.mode.clone());
            self.system.return_screen = Some(self.current_screen());
        }
        self.mode = AppMode::System;
        self.system.invalidate_log();
        debug!("opened System");
    }

    fn open_log_viewer(&mut self) {
        self.open_system();
        self.system.settings_tab = false;
        self.last_log_viewer_g = None;
        self.refresh_log_viewer_content();
        self.log_view_scroll = self.log_viewer_max_scroll();
    }

    fn close_log_viewer(&mut self) {
        if let Some(screen) = self.system.return_screen.take()
            && let Some(snapshot) = self.screen_states.get(&screen).cloned()
        {
            self.restore_screen(snapshot);
            self.last_log_viewer_g = None;
            return;
        }
        self.mode = self
            .system
            .return_mode
            .take()
            .or_else(|| self.previous_mode.take())
            .unwrap_or(AppMode::StartPage);
        self.last_log_viewer_g = None;
    }

    fn open_log_in_external_editor(&mut self) {
        debug!(path = %self.log_file_path.display(), "opening log file in external editor");
        let edit_result = external_editor::open_file_in_external_editor(&self.log_file_path);
        self.needs_terminal_refresh = true;

        match edit_result {
            Ok(()) => {
                self.system.invalidate_log();
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

        let relative = match parse_relative_query_path(&file_name) {
            Ok(path) => path,
            Err(message) => {
                self.status_message = message;
                return;
            }
        };

        let path = self.query_root_dir.join(&relative);
        if !is_supported_query_file(&path) {
            self.status_message =
                "Unsupported query file extension. Use js, mjs, sparql, sql, sjs, or xqy."
                    .to_string();
            return;
        }
        if !is_default_query_root(&self.query_root_dir)
            && let Err(message) = require_folder_for_query_file(&relative)
        {
            self.status_message = message;
            return;
        }
        if path.exists() {
            self.status_message = format!(
                "Query file already exists: {}",
                display_query_path(&path, &self.query_root_dir)
            );
            return;
        }
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            self.status_message = format!("Failed to create folder: {}", e);
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
        if let Some(row) = self.selected_query_row() {
            let name = display_query_path(&row.path, &self.query_root_dir);
            self.rename_file_input = name;
            self.rename_file_target = Some(row.path);
            self.open_modal(AppMode::QueryFileRename);
        }
    }

    /// Rename or move a file or folder. The new name may contain folders,
    /// which are created as needed. Folders are moved with their contents and
    /// result caches; empty folders left behind are removed on refresh.
    fn rename_query_file(&mut self) {
        let Some(old_path) = self.rename_file_target.take() else {
            self.close_modal_with_focus(Focus::Query);
            return;
        };
        let is_dir = old_path.is_dir();

        let relative = match parse_relative_query_path(&self.rename_file_input) {
            Ok(path) => path,
            Err(message) => {
                self.status_message = message;
                self.close_modal_with_focus(Focus::Query);
                return;
            }
        };
        if !is_dir && !is_supported_query_file(&relative) {
            self.status_message =
                "Renamed files must keep a supported extension: js, mjs, sjs, sql, sparql, or xqy."
                    .to_string();
            self.close_modal_with_focus(Focus::Query);
            return;
        }
        if !is_dir
            && !is_default_query_root(&self.query_root_dir)
            && let Err(message) = require_folder_for_query_file(&relative)
        {
            self.status_message = message;
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        let new_path = self.query_root_dir.join(&relative);
        if new_path == old_path {
            self.close_modal_with_focus(Focus::Query);
            return;
        }
        if is_dir && new_path.starts_with(&old_path) {
            self.status_message = "A folder cannot be moved into itself.".to_string();
            self.close_modal_with_focus(Focus::Query);
            return;
        }
        if new_path.exists() {
            self.status_message = format!(
                "Already exists: {}",
                display_query_path(&new_path, &self.query_root_dir)
            );
            self.close_modal_with_focus(Focus::Query);
            return;
        }
        // Running results are keyed by file path; moving them mid-run would
        // orphan the completion, so refuse instead.
        if self
            .running_queries
            .keys()
            .any(|path| path.starts_with(&old_path))
        {
            self.status_message =
                "Cannot rename while a query under this path is running.".to_string();
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        if let Some(parent) = new_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            self.status_message = format!("Failed to create folder: {}", e);
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        if let Err(e) = fs::rename(&old_path, &new_path) {
            self.status_message = format!("Failed to rename: {}", e);
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        if let Some(active) = self.active_query_file.clone()
            && let Some(moved) = remap_path_prefix(&active, &old_path, &new_path)
        {
            self.active_query_file = Some(moved);
        }
        self.collapsed_query_folders = std::mem::take(&mut self.collapsed_query_folders)
            .into_iter()
            .map(|folder| remap_path_prefix(&folder, &old_path, &new_path).unwrap_or(folder))
            .collect();
        remap_map_keys(&mut self.running_partial_results, &old_path, &new_path);

        let cache_move_warning = move_query_result_cache(
            &self.query_result_cache_dir,
            &self.query_root_dir,
            &old_path,
            &new_path,
        )
        .err()
        .map(|e| e.to_string());

        if let Err(e) = self.refresh_query_files() {
            self.status_message = format!("Renamed but failed to refresh list: {}", e);
        } else {
            self.select_query_path(&new_path);
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
        self.query_results_format = QueryResultsFormat::Plain;
        self.query_results_filter = None;
        self.uri_query_results.clear();
        self.query_uri_delete_target = None;
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
        self.query_results_format = QueryResultsFormat::Plain;
        self.query_results_filter = None;
        self.uri_query_results.clear();
        self.query_uri_delete_target = None;
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
            self.query_results_format = QueryResultsFormat::Plain;
            self.query_results_filter = None;
            self.uri_query_results.clear();
            self.query_uri_delete_target = None;
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
            if self.query_results.is_empty() {
                self.results_text = NO_OUTPUT_MESSAGE.to_string();
            }
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

    fn open_query_results_format_picker(&mut self) {
        self.query_results_format_picker_state
            .select(Some(self.query_results_format.picker_index()));
        self.open_modal(AppMode::QueryResultFormatSelect);
    }

    fn select_query_results_format(&mut self) {
        let Some(selected) = self.query_results_format_picker_state.selected() else {
            return;
        };
        self.close_modal_with_focus(Focus::Results);

        if selected == QueryResultsFormat::ALL.len() {
            self.edit_selected_query_result();
            return;
        }
        let Some(format) = QueryResultsFormat::ALL.get(selected).copied() else {
            return;
        };
        self.query_results_format = format;
        if format == QueryResultsFormat::Uris {
            self.load_uri_query_results();
        }
        self.status_message = format!("Query results format: {}", format.label());
    }

    fn load_uri_query_results(&mut self) {
        self.uri_query_results.clear();
        let Some(client) = self.client.clone() else {
            self.uri_query_results = self
                .visible_query_results()
                .into_iter()
                .map(|value| UriQueryResult {
                    uri: value.trim().to_string(),
                    collections: Vec::new(),
                    permissions: Vec::new(),
                    property_count: None,
                    error: Some("No server connected".to_string()),
                })
                .collect();
            self.status_message =
                "URI view: no server connected; showing URI values only".to_string();
            return;
        };

        let visible_uris = self
            .visible_query_results()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        for value in visible_uris {
            let uri = value.trim().to_string();
            if uri.is_empty() {
                self.uri_query_results.push(UriQueryResult {
                    uri,
                    collections: Vec::new(),
                    permissions: Vec::new(),
                    property_count: None,
                    error: Some("Empty result is not a URI".to_string()),
                });
                continue;
            }

            match self.rt.block_on(client.get_document_metadata(&uri)) {
                Ok(metadata) => {
                    let property_count =
                        self.rt.block_on(client.document_property_count(&uri)).ok();
                    self.uri_query_results.push(UriQueryResult {
                        uri,
                        collections: metadata.collections,
                        permissions: metadata.permissions,
                        property_count,
                        error: None,
                    });
                }
                Err(error) => self.uri_query_results.push(UriQueryResult {
                    uri,
                    collections: Vec::new(),
                    permissions: Vec::new(),
                    property_count: None,
                    error: Some(error.to_string()),
                }),
            }
        }
    }

    fn open_uri_query_result(&mut self) {
        let Some(selected) = self.query_results_state.selected() else {
            return;
        };
        let Some(uri_result) = self.uri_query_results.get(selected) else {
            return;
        };
        let uri = uri_result.uri.clone();
        if uri.is_empty() {
            self.status_message = "Select a non-empty URI first.".to_string();
            return;
        }

        if let Some(client) = self.client.clone() {
            match self.rt.block_on(client.get_document(&uri)) {
                Ok(document) => self.set_active_document(document),
                Err(error) => {
                    self.active_document = None;
                    self.full_view_content = format!("Error loading document {uri}: {error}");
                    self.full_view_scroll = 0;
                    self.mode = AppMode::FullScreenView;
                }
            }
        } else {
            self.active_document = None;
            self.full_view_content = format!("URI: {uri}\n\nNo server connected.");
            self.full_view_scroll = 0;
            self.mode = AppMode::FullScreenView;
        }
    }

    fn edit_selected_query_result(&mut self) {
        let Some(selected) = self.query_results_state.selected() else {
            return;
        };
        let Some(original_index) = self.visible_query_result_indices().get(selected).copied()
        else {
            return;
        };
        let Some(original) = self.query_results.get(original_index).cloned() else {
            return;
        };

        let edit_result =
            external_editor::edit_text_in_external_editor(&original, None, "query result");
        self.needs_terminal_refresh = true;
        match edit_result {
            Ok(edited) if edited != original => {
                self.query_results[original_index] = edited;
                self.uri_query_results.clear();
                self.query_results_timestamp = Some(SystemTime::now());
                if let Some(path) = self.active_query_file.clone() {
                    self.persist_query_results_for_path(&path);
                }
                self.status_message = "Query result updated from external editor.".to_string();
            }
            Ok(_) => self.status_message = "No query-result changes to save.".to_string(),
            Err(error) => self.status_message = format!("External edit failed: {error}"),
        }
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
        if self.running_queries.contains_key(&path) {
            // A query for this file is still running: show the latest
            // streamed partial results when available, otherwise the busy
            // state instead of stale cached results.
            if let Some(partial) = self.running_partial_results.get(&path).cloned() {
                self.clear_query_results_view();
                self.set_query_results(partial);
                self.query_results_timestamp = None;
            } else {
                self.show_query_busy();
            }
            self.query_dirty = false;
            self.query_last_edit = None;
            self.focus_query_panel();
            self.status_message = String::new();
            return Ok(());
        }
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
        // Keep Modules flags live: a saved working copy may have gone dirty.
        if self.module_status_for_working(&path).is_some() {
            self.refresh_cached_modules();
        }
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

    fn move_module_search_selection(&mut self, delta: isize) {
        if self.module_search.results.is_empty() {
            self.module_search.selected.select(None);
            return;
        }

        let len = self.module_search.results.len() as isize;
        let current = self
            .module_search
            .selected
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.module_search.selected.select(Some(next));
    }

    fn selected_module_uri(&self) -> Option<String> {
        let selected = self.module_search.selected.selected()?;
        self.module_search.results.get(selected).cloned()
    }

    fn open_module_picker(&mut self) {
        let Some(modules_db) = self.config.active_modules_database.clone() else {
            self.status_message =
                "No modules database selected. Use :servers and choose one under App Servers."
                    .to_string();
            return;
        };

        if self.client.is_none() {
            self.status_message = "No server connected.".to_string();
            return;
        }

        self.module_search.pattern.clear();
        self.module_search.results.clear();
        self.module_search.page = 0;
        self.module_search.total = None;
        self.module_search.focus_input = true;
        self.module_search.selected.select(None);
        self.module_search.marked.clear();
        self.status_message = format!(
            "Module search ready (database: {}). Type a pattern, Enter to search, Space to mark, S to download marked.",
            modules_db
        );
        self.open_modal(AppMode::ModuleCloneSelect);
    }

    fn search_modules(&mut self) {
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

        let start = self.module_search.page * self.module_search.page_size + 1;
        match self.rt.block_on(client.search_module_uris(
            &modules_db,
            &self.module_search.pattern,
            start,
            self.module_search.page_size,
        )) {
            Ok(result) => {
                self.module_search.results = result.uris;
                self.module_search.total = result.total;
                if self.module_search.results.is_empty() {
                    self.status_message = "No matching modules found.".to_string();
                    self.module_search.selected.select(None);
                } else {
                    let idx = self
                        .module_search
                        .selected
                        .selected()
                        .unwrap_or(0)
                        .min(self.module_search.results.len().saturating_sub(1));
                    self.module_search.selected.select(Some(idx));
                }
            }
            Err(e) => {
                self.status_message = format!("Failed to search module URIs: {}", e);
            }
        }
    }

    fn fetch_module_search_page(&mut self, delta: isize) {
        let max_page = self
            .module_search
            .total
            .map(|t| t.saturating_sub(1) / self.module_search.page_size)
            .unwrap_or(usize::MAX);
        let new_page = if delta >= 0 {
            (self.module_search.page + delta as usize).min(max_page)
        } else {
            self.module_search.page.saturating_sub(delta.unsigned_abs())
        };
        if new_page == self.module_search.page {
            return;
        }
        self.module_search.page = new_page;
        self.search_modules();
    }

    fn checkout_selected_module(&mut self) {
        // Picker Enter now saves the whole marked selection; a lone
        // highlighted row with no marks downloads just that row.
        if self.module_search.marked.is_empty() {
            if let Some(uri) = self.selected_module_uri() {
                self.module_search.marked.push(uri);
            }
        }
        self.save_marked_modules();
    }

    /// Toggle the mark on the highlighted search hit. Marks are keyed by
    /// URI, so they survive new searches and paging within this visit.
    fn toggle_mark_highlighted_module(&mut self) {
        let Some(uri) = self.selected_module_uri() else {
            self.status_message = "No module highlighted to mark.".to_string();
            return;
        };
        if let Some(pos) = self.module_search.marked.iter().position(|m| m == &uri) {
            self.module_search.marked.remove(pos);
            self.status_message = format!(
                "Unmarked {} ({} marked)",
                uri,
                self.module_search.marked.len()
            );
        } else {
            self.module_search.marked.push(uri.clone());
            self.status_message = format!(
                "Marked {} ({} marked)",
                uri,
                self.module_search.marked.len()
            );
        }
    }

    fn mark_all_visible_modules(&mut self) {
        let mut added = 0usize;
        for uri in self.module_search.results.clone() {
            if !self.module_search.marked.contains(&uri) {
                self.module_search.marked.push(uri);
                added += 1;
            }
        }
        self.status_message = if added == 0 {
            "All visible modules already marked.".to_string()
        } else {
            format!(
                "Marked {} module{} on this page ({} marked total)",
                added,
                if added == 1 { "" } else { "s" },
                self.module_search.marked.len()
            )
        };
    }

    fn clear_marked_modules(&mut self) {
        let count = self.module_search.marked.len();
        self.module_search.marked.clear();
        self.status_message = if count == 0 {
            "Nothing marked.".to_string()
        } else {
            format!(
                "Cleared {} marked module{}.",
                count,
                if count == 1 { "" } else { "s" }
            )
        };
    }

    /// Download every marked module, then close the picker into Modules view.
    fn save_marked_modules(&mut self) {
        if self.module_search.marked.is_empty() {
            self.status_message =
                "Nothing marked — Space to mark, Enter to download marked.".to_string();
            return;
        }
        let marked = self.module_search.marked.clone();
        let (ok, failed) = self.checkout_uris_hierarchical(&marked);
        if failed.is_empty() {
            self.status_message = format!(
                "Cached {} module{} (hierarchical, originals kept)",
                ok,
                if ok == 1 { "" } else { "s" }
            );
        } else {
            self.status_message = format!(
                "Cached {}; {} failed: {}",
                ok,
                failed.len(),
                failed.join("; ").chars().take(300).collect::<String>()
            );
        }
        self.module_search.marked.clear();
        self.mode = AppMode::Normal;
    }

    fn module_hidden_original_path(&self, path: &Path) -> PathBuf {
        self.modules_cache_dir()
            .join("legacy-originals")
            .join(path.file_name().unwrap_or_default())
    }

    /// Module working copies must never be shared merely because two
    /// environments use the same modules database name (usually `Modules`).
    /// Keep legacy/test callers without a client on the old local root; a
    /// connected environment gets a stable, opaque namespace.
    fn modules_cache_dir(&self) -> PathBuf {
        let Some(client) = self.client.as_ref() else {
            return self.query_result_cache_dir.clone();
        };
        let identity = format!(
            "{}\0{}\0{}",
            client.server.name,
            client.server.base_url(),
            client.server.username
        );
        self.query_result_cache_dir
            .join("module-environments")
            .join(modules_cache::content_hash(&identity))
    }

    fn active_sync_info(&self) -> Option<SyncInfo> {
        let content = self.query_text();
        parse_sync_comment(&content)
    }

    fn sync_active_module(&mut self) {
        let Some(sync) = self.active_sync_info() else {
            self.status_message = "No active synced module.".to_string();
            return;
        };

        let Some(client) = self.client.clone() else {
            self.status_message = "No server connected.".to_string();
            return;
        };

        let content = self.query_text();
        let body = strip_sync_comment(&content);
        match self
            .rt
            .block_on(client.update_document_in_modules_database(&sync.uri, &body, &sync.db))
        {
            Ok(()) => {
                self.status_message = format!("Synced module {} to {}", sync.uri, sync.db);
            }
            Err(e) => {
                self.status_message = format!("Failed to sync module: {}", e);
            }
        }
    }

    fn reset_remote_module(&mut self) {
        let Some(sync) = self.active_sync_info() else {
            self.status_message = "No active synced module.".to_string();
            return;
        };

        let Some(path) = self.active_query_file.clone() else {
            return;
        };

        let hidden_original = self.module_hidden_original_path(&path);
        if !hidden_original.exists() {
            self.status_message = "Hidden original module copy is missing.".to_string();
            return;
        }

        let original = match fs::read_to_string(&hidden_original) {
            Ok(content) => content,
            Err(e) => {
                self.status_message = format!("Failed to read hidden original: {}", e);
                return;
            }
        };

        let Some(client) = self.client.clone() else {
            self.status_message = "No server connected.".to_string();
            return;
        };

        match self
            .rt
            .block_on(client.update_document_in_modules_database(&sync.uri, &original, &sync.db))
        {
            Ok(()) => {
                self.status_message = format!("Reset remote module {} in {}", sync.uri, sync.db);
                self.open_modal(AppMode::ModuleResetConfirm);
            }
            Err(e) => {
                self.status_message = format!("Failed to reset module: {}", e);
            }
        }
    }

    fn drop_active_module_file(&mut self, reset_remote: bool) {
        let Some(path) = self.active_query_file.take() else {
            return;
        };
        let hidden_original = self.module_hidden_original_path(&path);

        if reset_remote {
            let Some(sync) = self.active_sync_info() else {
                self.close_modal_restore_focus();
                return;
            };
            let Some(client) = self.client.clone() else {
                self.status_message = "No server connected.".to_string();
                self.close_modal_restore_focus();
                return;
            };
            let original = match fs::read_to_string(&hidden_original) {
                Ok(content) => content,
                Err(e) => {
                    self.status_message = format!("Failed to read hidden original: {}", e);
                    self.close_modal_restore_focus();
                    return;
                }
            };
            if let Err(e) = self.rt.block_on(
                client.update_document_in_modules_database(&sync.uri, &original, &sync.db),
            ) {
                self.status_message = format!("Failed to reset remote module: {}", e);
                self.close_modal_restore_focus();
                return;
            }
        }

        let _ = fs::remove_file(&hidden_original);
        if let Err(e) = fs::remove_file(&path) {
            self.status_message = format!("Failed to drop local module: {}", e);
            self.close_modal_restore_focus();
            return;
        }

        self.query_editor = Self::new_query_editor(Vec::new());
        self.active_query_file = None;
        self.query_dirty = false;
        self.query_last_edit = None;
        let _ = self.refresh_query_files();
        self.status_message = "Dropped local module checkout.".to_string();
        self.close_modal_restore_focus();
    }

    fn confirm_module_reset_drop_local(&mut self, drop_local: bool) {
        self.close_modal_restore_focus();
        if drop_local {
            self.drop_active_module_file(false);
        }
    }

    fn drop_local_module(&mut self) {
        let Some(_sync) = self.active_sync_info() else {
            self.status_message = "No active synced module.".to_string();
            return;
        };

        let Some(path) = self.active_query_file.clone() else {
            return;
        };

        let hidden_original = self.module_hidden_original_path(&path);
        if !hidden_original.exists() {
            self.status_message = "Hidden original module copy is missing.".to_string();
            return;
        }

        let local_body = strip_sync_comment(&self.query_text());
        let original = match fs::read_to_string(&hidden_original) {
            Ok(content) => content,
            Err(e) => {
                self.status_message = format!("Failed to read hidden original: {}", e);
                return;
            }
        };

        if local_body != original {
            self.module_drop_needs_reset = true;
            self.open_modal(AppMode::ModuleDropConfirm);
        } else {
            self.drop_active_module_file(false);
        }
    }

    fn confirm_module_drop(&mut self, reset_remote: bool) {
        self.drop_active_module_file(reset_remote);
    }

    fn active_modules_db(&self) -> Option<String> {
        self.config.active_modules_database.clone()
    }

    fn toggle_modules_view(&mut self) {
        self.modules_view = !self.modules_view;
        if self.modules_view {
            self.file_list_visible = true;
            self.refresh_cached_modules();
            // Land the selection on the file actually in the editor when it
            // is a cached module, so the highlight matches what you edit.
            if let Some(active) = self.active_query_file.clone()
                && self.module_status_for_working(&active).is_some()
            {
                self.reveal_module_path(&active);
            }
            let count = self.cached_modules.len();
            self.status_message = format!(
                "Modules view: {} cached module{}. [Enter] open · [e] edit · [d] diff · [s] sync · [r] reset · [n] new · [D] drop · [T] files",
                count,
                if count == 1 { "" } else { "s" }
            );
        } else {
            // Point the Files selection at the editor contents so the
            // highlight never disagrees with the file being edited.
            self.sync_query_selection_to_active();
            self.status_message = "Files view.".to_string();
        }
    }

    fn refresh_cached_modules(&mut self) {
        let Some(db) = self.active_modules_db() else {
            self.cached_modules.clear();
            self.cached_modules_state.select(None);
            return;
        };
        // Capture the selected URI before replacing the statuses.
        let current_uri = self
            .selected_module_row()
            .filter(|row| !row.is_dir)
            .and_then(|row| self.module_status_for_working(&row.path))
            .map(|status| status.uri);
        let cache_dir = self.modules_cache_dir();
        let manifest = modules_cache::load_manifest(&cache_dir, &db).unwrap_or_default();
        self.cached_modules = modules_cache::compute_statuses(&cache_dir, &db, &manifest);
        if self.cached_modules.is_empty() {
            self.cached_modules_state.select(None);
        } else {
            // Keep the selection on the previously selected URI when it is
            // still visible, else on the active file, else the first row.
            let rows = self.module_tree_rows();
            let uri_row = |uri: &str| {
                rows.iter().position(|row| {
                    !row.is_dir
                        && self
                            .module_status_for_working(&row.path)
                            .is_some_and(|status| status.uri == uri)
                })
            };
            let selected = current_uri
                .as_deref()
                .and_then(uri_row)
                .or_else(|| {
                    self.active_query_file.as_ref().and_then(|active| {
                        rows.iter()
                            .position(|row| !row.is_dir && row.path == *active)
                    })
                })
                .or(Some(0));
            self.cached_modules_state.select(selected);
        }
    }

    /// Root of the hierarchical modules cache for the active modules DB.
    fn module_cache_root(&self) -> Option<PathBuf> {
        self.active_modules_db()
            .map(|db| modules_cache::modules_db_root(&self.modules_cache_dir(), &db))
    }

    /// Visible rows of the Modules tree, mirroring the Files tree: folders
    /// first at each level, children hidden while their folder is collapsed.
    fn module_tree_rows(&self) -> Vec<QueryTreeRow> {
        self.module_tree_rows_with(&self.collapsed_module_folders)
    }

    /// Modules tree rows for an arbitrary collapsed set. Used with an empty set
    /// to measure the widest label regardless of folding.
    fn module_tree_rows_with(&self, collapsed: &HashSet<PathBuf>) -> Vec<QueryTreeRow> {
        let Some(root) = self.module_cache_root() else {
            return Vec::new();
        };
        let mut files = Vec::new();
        let mut folders: HashSet<PathBuf> = HashSet::new();
        for entry in &self.cached_modules {
            files.push(entry.working_path.clone());
            let mut dir = entry.working_path.parent();
            while let Some(parent) = dir {
                if parent == root {
                    break;
                }
                // Guard against entries that escaped the cache root.
                if !parent.starts_with(&root) {
                    break;
                }
                folders.insert(parent.to_path_buf());
                dir = parent.parent();
            }
        }
        let folders: Vec<PathBuf> = folders.into_iter().collect();
        build_query_tree_rows(&root, &files, &folders, collapsed)
    }

    fn selected_module_row(&self) -> Option<QueryTreeRow> {
        let selected = self.cached_modules_state.selected()?;
        self.module_tree_rows().into_iter().nth(selected)
    }

    fn module_status_for_working(&self, path: &Path) -> Option<modules_cache::CachedModuleStatus> {
        self.cached_modules
            .iter()
            .find(|entry| entry.working_path == path)
            .cloned()
    }

    /// Select a module tree row. If it is a file, load it into the editor —
    /// visiting a row updates the editor, just like the Files tree.
    fn open_module_row(&mut self, index: usize) {
        let rows = self.module_tree_rows();
        let Some(row) = rows.get(index) else {
            return;
        };
        let path = row.path.clone();
        let is_dir = row.is_dir;
        self.cached_modules_state.select(Some(index));
        if is_dir {
            return;
        }
        let Some(status) = self.module_status_for_working(&path) else {
            return;
        };
        self.load_cached_module_into_editor(&status.uri);
    }

    /// Expand or collapse a Modules folder and keep the selection on it.
    fn toggle_module_folder(&mut self, path: &Path) {
        if !self.collapsed_module_folders.remove(path) {
            self.collapsed_module_folders.insert(path.to_path_buf());
        }
        self.select_module_path(path);
    }

    /// Expand collapsed ancestors of `path` and select its Modules row,
    /// so a reveal always lands on a visible row.
    fn reveal_module_path(&mut self, path: &Path) {
        let root = self.module_cache_root();
        let mut dir = path.parent();
        while let Some(parent) = dir {
            self.collapsed_module_folders.remove(parent);
            if root.as_deref() == Some(parent) {
                break;
            }
            dir = parent.parent();
        }
        self.select_module_path(path);
    }

    /// Select the Modules row for `path` if it is visible.
    fn select_module_path(&mut self, path: &Path) {
        let index = self
            .module_tree_rows()
            .iter()
            .position(|row| row.path == path);
        match index {
            Some(index) => self.cached_modules_state.select(Some(index)),
            None => self.cached_modules_state.select(Some(0)),
        }
    }

    fn move_cached_module_selection(&mut self, delta: isize) {
        let rows = self.module_tree_rows();
        if rows.is_empty() {
            self.cached_modules_state.select(None);
            return;
        }
        let len = rows.len() as isize;
        let current = self
            .cached_modules_state
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.open_module_row(next);
    }

    fn selected_cached_module(&self) -> Option<modules_cache::CachedModuleStatus> {
        let row = self.selected_module_row()?;
        if row.is_dir {
            return None;
        }
        self.module_status_for_working(&row.path)
    }

    fn load_cached_module_into_editor(&mut self, uri: &str) {
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            return;
        };
        let working = modules_cache::working_path_for(&self.modules_cache_dir(), &db, uri);
        if !working.exists() {
            self.status_message = format!("Cached file missing for {}", uri);
            return;
        }
        if let Err(e) = self.save_query_file_if_dirty() {
            self.status_message = format!("Save error: {}", e);
            return;
        }
        match self.load_query_file(working.clone()) {
            Ok(()) => {
                self.status_message = format!("Loaded cached module {}", uri);
            }
            Err(e) => {
                self.status_message = format!("Failed to load {}: {}", uri, e);
            }
        }
        // Keep the cached list selection on this uri.
        if let Some(idx) = self.module_tree_rows().iter().position(|row| {
            !row.is_dir
                && self
                    .module_status_for_working(&row.path)
                    .is_some_and(|status| status.uri == uri)
        }) {
            self.cached_modules_state.select(Some(idx));
        }
    }

    fn open_cached_module_in_external_editor(&mut self) {
        let Some(entry) = self.selected_cached_module() else {
            self.status_message = "No cached module selected.".to_string();
            return;
        };
        // Load into the query pane first so save-back targets the working copy.
        self.load_cached_module_into_editor(&entry.uri);
        self.open_query_in_external_editor();
        self.refresh_cached_modules();
    }

    fn show_cached_module_diff(&mut self) {
        let Some(entry) = self.selected_cached_module() else {
            self.status_message = "No cached module selected.".to_string();
            return;
        };
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            return;
        };
        let cache_dir = self.modules_cache_dir();
        let working = modules_cache::working_path_for(&cache_dir, &db, &entry.uri);
        let original = modules_cache::original_path_for(&cache_dir, &db, &entry.uri);
        let original_content = fs::read_to_string(&original).unwrap_or_default();
        // Prefer unsaved editor contents when the working file is active.
        let modified = if self.active_query_file.as_ref() == Some(&working) {
            self.query_text()
        } else {
            fs::read_to_string(&working).unwrap_or_default()
        };
        if original_content == modified {
            self.status_message = format!("No local changes for {}", entry.uri);
            return;
        }
        self.module_diff_uri = Some(entry.uri.clone());
        self.module_diff_lines = modules_cache::unified_diff(&original_content, &modified);
        self.module_diff_scroll = 0;
        self.open_modal(AppMode::ModuleDiffView);
    }

    fn sync_all_cached_modules(&mut self) {
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            return;
        };
        let Some(client) = self.client.clone() else {
            self.status_message = "No server connected.".to_string();
            return;
        };
        if let Err(e) = self.save_query_file_if_dirty() {
            self.status_message = format!("Save error before sync: {}", e);
            return;
        }
        let cache_dir = self.modules_cache_dir();
        let manifest = modules_cache::load_manifest(&cache_dir, &db).unwrap_or_default();
        let statuses = modules_cache::compute_statuses(&cache_dir, &db, &manifest);
        let pending: Vec<_> = statuses.into_iter().filter(|s| s.unsynced).collect();
        if pending.is_empty() {
            self.status_message = "All cached modules already synced.".to_string();
            return;
        }
        let mut synced = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for entry in &pending {
            let content = match fs::read_to_string(&entry.working_path) {
                Ok(c) => c,
                Err(e) => {
                    failed.push(format!("{}: {}", entry.uri, e));
                    continue;
                }
            };
            match self
                .rt
                .block_on(client.update_document_in_modules_database(&entry.uri, &content, &db))
            {
                Ok(()) => {
                    if let Err(e) =
                        modules_cache::mark_synced(&cache_dir, &db, &entry.uri, &content)
                    {
                        failed.push(format!("{}: manifest: {}", entry.uri, e));
                    } else {
                        synced += 1;
                    }
                }
                Err(e) => failed.push(format!("{}: {}", entry.uri, e)),
            }
        }
        self.refresh_cached_modules();
        self.status_message = if failed.is_empty() {
            format!(
                "Synced {} module{}.",
                synced,
                if synced == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "Synced {}; {} failed: {}",
                synced,
                failed.len(),
                failed.join("; ").chars().take(300).collect::<String>()
            )
        };
    }

    /// Reset the selected module from the active server. The remote version
    /// becomes both the refreshed original and the clean working copy; reset
    /// never overwrites one environment with an older local checkout.
    fn reset_selected_to_original(&mut self) {
        // Recompute first: statuses go stale after edits, and reset must
        // decide on fresh dirty state.
        self.refresh_cached_modules();
        let Some(entry) = self.selected_cached_module() else {
            self.status_message = "No cached module selected.".to_string();
            return;
        };
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            return;
        };
        let Some(client) = self.client.clone() else {
            self.status_message = "No server connected.".to_string();
            return;
        };
        let remote_content = match self
            .rt
            .block_on(client.get_document_content_from_database(&entry.uri, &db))
        {
            Ok(content) => content,
            Err(error) => {
                self.status_message = format!("Failed to fetch {}: {}", entry.uri, error);
                return;
            }
        };
        let cache_dir = self.modules_cache_dir();
        let original = modules_cache::original_path_for(&cache_dir, &db, &entry.uri);
        let changed_on_server = fs::read_to_string(&original)
            .map(|original| original != remote_content)
            .unwrap_or(true);
        if let Err(error) =
            modules_cache::checkout_module(&cache_dir, &db, &entry.uri, &remote_content)
        {
            self.status_message =
                format!("Failed to refresh original for {}: {}", entry.uri, error);
            return;
        }
        match modules_cache::apply_reset_to_original(&cache_dir, &db, &entry.uri, &remote_content) {
            Ok(working) => {
                if self.active_query_file.as_ref() == Some(&working)
                    && let Err(e) = self.load_query_file(working)
                {
                    self.status_message = format!("Reset {} but reload failed: {}", entry.uri, e);
                    return;
                }
                self.refresh_cached_modules();
                self.status_message = format!(
                    "Reset {} from the {} server version.",
                    entry.uri,
                    if changed_on_server {
                        "changed"
                    } else {
                        "current"
                    },
                );
            }
            Err(e) => {
                self.status_message =
                    format!("Fetched {} but local reset failed: {}", entry.uri, e);
            }
        }
    }

    /// Start the drop-cache flow: confirm, warning when local content exists
    /// only here (dirty and/or never synced).
    fn request_drop_cached_selected(&mut self) {
        // Fresh flags: the confirm warning must reflect current files.
        self.refresh_cached_modules();
        let Some(entry) = self.selected_cached_module() else {
            self.status_message = "No cached module selected.".to_string();
            return;
        };
        self.module_drop_target = Some((entry.uri.clone(), entry.dirty, entry.unsynced));
        self.open_modal(AppMode::ModuleDropCacheConfirm);
    }

    fn confirm_drop_cached_selected(&mut self) {
        let Some((uri, _, _)) = self.module_drop_target.take() else {
            self.close_modal_restore_focus();
            return;
        };
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            self.close_modal_restore_focus();
            return;
        };
        let cache_dir = self.modules_cache_dir();
        match modules_cache::drop_cached_module(&cache_dir, &db, &uri) {
            Ok(()) => {
                // If the editor showed the dropped file, fall back to files.
                if let Some(active) = self.active_query_file.clone()
                    && modules_cache::working_path_for(&cache_dir, &db, &uri) == active
                {
                    self.active_query_file = None;
                    self.query_editor = Self::new_query_editor(Vec::new());
                    self.query_dirty = false;
                    self.query_last_edit = None;
                    self.clear_query_results_view();
                }
                let _ = self.refresh_query_files();
                self.refresh_cached_modules();
                self.status_message =
                    format!("Dropped local cached copy of {} (server untouched)", uri);
            }
            Err(e) => {
                self.status_message = format!("Failed to drop {}: {}", uri, e);
            }
        }
        self.close_modal_restore_focus();
    }

    fn open_module_create(&mut self) {
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            return;
        };
        if self.client.is_none() {
            self.status_message = "No server connected.".to_string();
            return;
        }
        self.new_module_uri_input.clear();
        self.status_message = format!(
            "New module in {}: enter a URI like /endpoints/my/get.xqy",
            db
        );
        self.open_modal(AppMode::ModuleCreate);
    }

    fn create_module_from_input(&mut self) {
        let input = self.new_module_uri_input.trim().to_string();
        let uri = match modules_cache::validate_new_module_uri(&input) {
            Ok(uri) => uri,
            Err(e) => {
                self.status_message = format!("Invalid module URI: {}", e);
                return;
            }
        };
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            return;
        };
        let cache_dir = self.modules_cache_dir();
        // Already cached: reveal and load it instead of overwriting.
        if self
            .module_status_for_working(&modules_cache::working_path_for(&cache_dir, &db, &uri))
            .is_some()
        {
            self.refresh_cached_modules();
            self.close_modal_with_focus(Focus::Query);
            self.reveal_active_cached_module(&uri);
            self.load_cached_module_into_editor(&uri);
            self.status_message = format!("Already cached: {}", uri);
            return;
        }
        match modules_cache::stage_new_module(&cache_dir, &db, &uri) {
            Ok(working) => {
                self.refresh_cached_modules();
                self.close_modal_with_focus(Focus::Query);
                self.reveal_module_path(&working);
                self.load_cached_module_into_editor(&uri);
                self.status_message = format!(
                    "Created {} locally — write it, then 's' syncs it to {}",
                    uri, db
                );
            }
            Err(e) => {
                self.status_message = format!("Failed to create {}: {}", uri, e);
            }
        }
    }

    /// Select the cached row for `uri`, expanding collapsed ancestors.
    fn reveal_active_cached_module(&mut self, uri: &str) {
        let Some(db) = self.active_modules_db() else {
            return;
        };
        let working = modules_cache::working_path_for(&self.modules_cache_dir(), &db, uri);
        self.reveal_module_path(&working);
    }

    /// Fetch remote content for each uri and write hierarchical working+original copies.
    /// Returns (checked_out_count, failures). Working copies with local edits are kept.
    fn checkout_uris_hierarchical(&mut self, uris: &[String]) -> (usize, Vec<String>) {
        let Some(db) = self.active_modules_db() else {
            self.status_message = "No modules database selected.".to_string();
            return (0, vec!["no modules database".to_string()]);
        };
        let Some(client) = self.client.clone() else {
            self.status_message = "No server connected.".to_string();
            return (0, vec!["no server connected".to_string()]);
        };
        let cache_dir = self.modules_cache_dir();
        let mut ok = 0usize;
        let mut failed = Vec::new();
        let mut last_working: Option<PathBuf> = None;
        for uri in uris {
            match self
                .rt
                .block_on(client.get_document_content_from_database(uri, &db))
            {
                Ok(remote_content) => {
                    match modules_cache::checkout_module(&cache_dir, &db, uri, &remote_content) {
                        Ok(working) => {
                            ok += 1;
                            last_working = Some(working);
                        }
                        Err(e) => failed.push(format!("{}: {}", uri, e)),
                    }
                }
                Err(e) => failed.push(format!("{}: {}", uri, e)),
            }
        }
        self.refresh_cached_modules();
        if let Some(working) = last_working.filter(|_| ok > 0 && uris.len() == 1) {
            // Single checkout loads straight into the editor for immediate work.
            if let Err(e) = self.load_query_file(working.clone()) {
                failed.push(format!("load {}: {}", working.display(), e));
            } else {
                self.modules_view = true;
            }
        } else if ok > 0 {
            self.modules_view = true;
        }
        (ok, failed)
    }

    fn status_line(&self) -> Line<'static> {
        let mode_label = format!(" {} ", self.status_mode_label());
        let mode_span = Span::styled(mode_label, self.edit_mode_style());
        let database = self
            .config
            .active_database
            .as_deref()
            .unwrap_or("(no database)");
        let spans = vec![
            Span::styled(
                format!("{} ", self.environment_indicator()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            mode_span,
            Span::raw(format!(" · {database}")),
        ];
        Line::from(spans)
    }

    fn environment_indicator(&self) -> String {
        let server = self
            .config
            .active_server
            .as_deref()
            .unwrap_or("(no server)");
        let port = self
            .config
            .effective_active_server_config()
            .map(|server| server.port.to_string())
            .unwrap_or_else(|| "-".into());
        format!("{server}:{port} ▾")
    }

    fn open_server_picker(&mut self) {
        self.server_picker_open = true;
        self.server_picker_error = None;
        self.server_picker_state.select(
            self.config
                .servers
                .iter()
                .position(|server| self.config.active_server.as_deref() == Some(&server.name))
                .or_else(|| (!self.config.servers.is_empty()).then_some(0)),
        );
    }

    fn activate_picker_server(&mut self) {
        let Some(name) = self
            .server_picker_state
            .selected()
            .and_then(|index| self.config.servers.get(index))
            .map(|server| server.name.clone())
        else {
            return;
        };
        if self.config.active_server.as_deref() == Some(name.as_str()) {
            self.server_picker_open = false;
            return;
        }
        if self.activate_server(name) {
            self.server_picker_open = false;
        } else {
            self.server_picker_error = Some("Switch failed; server unchanged".into());
        }
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
            "logs" => {
                self.server_logs.open(
                    self.client.clone(),
                    _arg.unwrap_or(""),
                    previous_mode,
                    &self.rt,
                );
                self.mode = AppMode::MarkLogicLogs;
            }
            "reset" => {
                self.server_logs.reset_time_tracking();
                self.server_logs
                    .resume(self.client.clone(), previous_mode, &self.rt);
                self.mode = AppMode::MarkLogicLogs;
            }
            "reports" => self.open_reports(_arg.unwrap_or("")),
            "system" => {
                self.mode = previous_mode;
                self.open_system();
            }
            "app-logs" => {
                self.mode = previous_mode;
                self.open_log_viewer();
            }
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
        // A modules working copy belongs to the selected environment.  The
        // list is otherwise retained in memory until the user toggles the
        // side pane, which briefly presents the previous environment's
        // modules after a server switch.
        self.refresh_cached_modules();
    }

    fn cmd_servers(&mut self) {
        self.open_servers_interface();
    }

    fn restored_servers_row_index(
        rows: &[ServersInterfaceRow],
        preferred: Option<&ServersInterfaceRow>,
    ) -> Option<usize> {
        preferred
            .and_then(|row| rows.iter().position(|candidate| candidate == row))
            .or_else(|| (!rows.is_empty()).then_some(0))
    }

    fn rebuild_servers_interface_rows(&mut self) {
        let selected_favorite = self.selected_servers_interface_row();
        let selected_tree = if self.servers_interface_focus == ServersInterfaceFocus::Favorites {
            self.servers_interface_tree_state
                .selected()
                .and_then(|index| self.servers_interface_tree.get(index).cloned())
        } else {
            self.selected_servers_interface_row()
        };

        self.servers_interface_favorites = self
            .config
            .servers
            .iter()
            .flat_map(|server| {
                server
                    .app_servers
                    .iter()
                    .filter(|app_server| app_server.favorite)
                    .map(move |app_server| ServersInterfaceRow::AppServer {
                        server_name: server.name.clone(),
                        group_name: app_server.group.clone(),
                        app_server_name: app_server.name.clone(),
                    })
            })
            .collect();

        self.servers_interface_tree = self
            .config
            .servers
            .iter()
            .flat_map(|server| {
                let mut rows = vec![ServersInterfaceRow::Server {
                    server_name: server.name.clone(),
                }];
                if self.expanded_servers.contains(&server.name) {
                    let mut groups = Vec::new();
                    for app_server in &server.app_servers {
                        if !groups.contains(&app_server.group) {
                            groups.push(app_server.group.clone());
                        }
                    }
                    for group_name in groups {
                        rows.push(ServersInterfaceRow::Group {
                            server_name: server.name.clone(),
                            group_name: group_name.clone(),
                        });
                        rows.extend(
                            server
                                .app_servers
                                .iter()
                                .filter(|app_server| app_server.group == group_name)
                                .map(|app_server| ServersInterfaceRow::AppServer {
                                    server_name: server.name.clone(),
                                    group_name: app_server.group.clone(),
                                    app_server_name: app_server.name.clone(),
                                }),
                        );
                    }
                }
                rows
            })
            .collect();

        let selected_favorite = Self::restored_servers_row_index(
            &self.servers_interface_favorites,
            selected_favorite.as_ref(),
        );
        self.servers_interface_favorites_state
            .select(selected_favorite);
        let selected_tree =
            Self::restored_servers_row_index(&self.servers_interface_tree, selected_tree.as_ref());
        self.servers_interface_tree_state.select(selected_tree);
    }

    fn selected_servers_interface_row(&self) -> Option<ServersInterfaceRow> {
        match self.servers_interface_focus {
            ServersInterfaceFocus::Favorites => self
                .servers_interface_favorites_state
                .selected()
                .and_then(|index| self.servers_interface_favorites.get(index).cloned()),
            ServersInterfaceFocus::Tree => self
                .servers_interface_tree_state
                .selected()
                .and_then(|index| self.servers_interface_tree.get(index).cloned()),
        }
    }

    fn selected_server_name(&self) -> Option<String> {
        match self.selected_servers_interface_row()? {
            ServersInterfaceRow::Server { server_name } => Some(server_name),
            ServersInterfaceRow::Group { server_name, .. } => Some(server_name),
            ServersInterfaceRow::AppServer { server_name, .. } => Some(server_name),
        }
    }

    fn selected_app_server_key(&self) -> Option<(String, String, String)> {
        match self.selected_servers_interface_row()? {
            ServersInterfaceRow::AppServer {
                server_name,
                group_name,
                app_server_name,
            } => Some((server_name, group_name, app_server_name)),
            ServersInterfaceRow::Server { .. } | ServersInterfaceRow::Group { .. } => None,
        }
    }

    fn app_server_key(group: &str, name: &str) -> String {
        format!("{}\u{0}{}", group, name)
    }

    fn move_servers_interface_selection(&mut self, delta: isize) {
        let (len, current) = match self.servers_interface_focus {
            ServersInterfaceFocus::Favorites => (
                self.servers_interface_favorites.len(),
                self.servers_interface_favorites_state.selected(),
            ),
            ServersInterfaceFocus::Tree => (
                self.servers_interface_tree.len(),
                self.servers_interface_tree_state.selected(),
            ),
        };
        if len == 0 {
            match self.servers_interface_focus {
                ServersInterfaceFocus::Favorites => {
                    self.servers_interface_favorites_state.select(None)
                }
                ServersInterfaceFocus::Tree => self.servers_interface_tree_state.select(None),
            }
            return;
        }

        let len = len as isize;
        let current = current
            .map(|index| index as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let selected = Some((current + delta).clamp(0, len - 1) as usize);
        match self.servers_interface_focus {
            ServersInterfaceFocus::Favorites => {
                self.servers_interface_favorites_state.select(selected)
            }
            ServersInterfaceFocus::Tree => self.servers_interface_tree_state.select(selected),
        }
    }

    fn select_edge_servers_interface_item(&mut self, first: bool) {
        let (len, selected) = match self.servers_interface_focus {
            ServersInterfaceFocus::Favorites => (
                self.servers_interface_favorites.len(),
                (!self.servers_interface_favorites.is_empty()).then_some(0),
            ),
            ServersInterfaceFocus::Tree => (
                self.servers_interface_tree.len(),
                (!self.servers_interface_tree.is_empty()).then_some(0),
            ),
        };
        let selected = if len == 0 {
            None
        } else if first {
            selected
        } else {
            Some(len - 1)
        };
        match self.servers_interface_focus {
            ServersInterfaceFocus::Favorites => {
                self.servers_interface_favorites_state.select(selected)
            }
            ServersInterfaceFocus::Tree => self.servers_interface_tree_state.select(selected),
        }
    }

    fn toggle_selected_server_expansion(&mut self) {
        let Some(ServersInterfaceRow::Server { server_name }) =
            self.selected_servers_interface_row()
        else {
            return;
        };
        if !self.expanded_servers.remove(&server_name) {
            self.expanded_servers.insert(server_name.clone());
        }
        self.rebuild_servers_interface_rows();
    }

    fn toggle_servers_interface_focus(&mut self) {
        self.servers_interface_focus = match self.servers_interface_focus {
            ServersInterfaceFocus::Favorites => ServersInterfaceFocus::Tree,
            ServersInterfaceFocus::Tree => ServersInterfaceFocus::Favorites,
        };
    }

    fn open_servers_interface(&mut self) {
        self.rebuild_servers_interface_rows();
        self.servers_interface_focus = if self.servers_interface_favorites.is_empty() {
            ServersInterfaceFocus::Tree
        } else {
            ServersInterfaceFocus::Favorites
        };
        self.open_interface(AppInterface::Servers);
        if self.config.servers.is_empty() {
            self.status_message = "No servers configured. Press 'a' to add one.".to_string();
        }
    }

    fn activate_selected_server(&mut self) {
        let Some(name) = self.selected_server_name() else {
            self.status_message = "No server selected.".to_string();
            return;
        };
        self.activate_server(name);
    }

    fn activate_server(&mut self, name: String) -> bool {
        if let Err(error) = self.save_query_file_if_dirty() {
            self.status_message = format!("Could not switch server: {error:#}");
            return false;
        }
        let previous = self.config.active_server.clone();
        self.config.active_server = Some(name.clone());
        match self.save_app_config() {
            Ok(()) => {
                self.reconnect();
                self.current_collection = None;
                self.current_page = 0;
                self.rebuild_servers_interface_rows();
                self.status_message = format!("Switched to server: {}", name);
                true
            }
            Err(e) => {
                self.status_message = format!("Failed to save active server: {}", e);
                self.config.active_server = previous;
                let _ = self.config.save();
                false
            }
        }
    }

    fn activate_selected_app_server(&mut self) {
        let Some((server_name, group_name, app_server_name)) = self.selected_app_server_key()
        else {
            self.status_message = "No app server selected.".to_string();
            return;
        };
        let app_server = self
            .config
            .servers
            .iter()
            .find(|server| server.name == server_name)
            .and_then(|server| {
                server.app_servers.iter().find(|app_server| {
                    app_server.group == group_name && app_server.name == app_server_name
                })
            })
            .cloned();
        let Some(app_server) = app_server else {
            self.status_message = "No app server selected.".to_string();
            return;
        };

        self.config.active_server = Some(server_name.clone());
        self.config.active_app_server = Some(app_server.name.clone());
        self.config.active_database = app_server.content_database.clone();
        self.config.active_modules_database = app_server.modules_database.clone();
        if let Some(server) = self
            .config
            .servers
            .iter_mut()
            .find(|server| server.name == server_name)
        {
            server.port = app_server.port;
            server.secure = app_server.secure;
        }

        match self.save_app_config() {
            Ok(()) => {
                self.reconnect();
                self.rebuild_servers_interface_rows();
                self.status_message = format!(
                    "App server: {} / {} / {}:{} ({})",
                    server_name,
                    group_name,
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
        let current = self.server_form_fields[4]
            .parse::<AuthType>()
            .unwrap_or(AuthType::Digest);
        let idx = AuthType::VARIANTS
            .iter()
            .position(|v| v == &current)
            .unwrap_or(0);
        let next = AuthType::VARIANTS[(idx + 1) % AuthType::VARIANTS.len()];
        self.server_form_fields[4] = next.to_string();
    }

    fn cycle_auth_type_prev(&mut self) {
        let current = self.server_form_fields[4]
            .parse::<AuthType>()
            .unwrap_or(AuthType::Digest);
        let idx = AuthType::VARIANTS
            .iter()
            .position(|v| v == &current)
            .unwrap_or(0);
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

    fn open_app_server_add(&mut self) {
        let context = match self.selected_servers_interface_row() {
            Some(ServersInterfaceRow::Server { server_name }) => {
                Some((server_name, client::default_app_server_group()))
            }
            Some(ServersInterfaceRow::Group {
                server_name,
                group_name,
            }) => Some((server_name, group_name)),
            Some(ServersInterfaceRow::AppServer {
                server_name,
                group_name,
                ..
            }) => Some((server_name, group_name)),
            None => None,
        };
        let Some((server_name, group_name)) = context else {
            self.status_message = "Select a server before adding an app server.".to_string();
            return;
        };
        self.app_server_list_server_name = Some(server_name);
        self.app_server_edit_group = Some(group_name);
        self.app_server_form_mode = ServerFormMode::Add;
        self.app_server_form_step = 0;
        self.app_server_form_fields = vec![String::new(); 5];
        self.app_server_form_fields[2] = "n".to_string();
        self.app_server_edit_target = None;
        self.mode = AppMode::AppServerForm;
    }

    fn open_app_server_edit(&mut self) {
        let Some((server_name, group_name, app_server_name)) = self.selected_app_server_key()
        else {
            self.status_message = "No app server selected.".to_string();
            return;
        };
        let endpoint = self
            .config
            .servers
            .iter()
            .find(|server| server.name == server_name)
            .and_then(|server| {
                server.app_servers.iter().find(|app_server| {
                    app_server.group == group_name && app_server.name == app_server_name
                })
            })
            .cloned();
        let Some(endpoint) = endpoint else {
            self.status_message = "No app server selected.".to_string();
            return;
        };

        self.app_server_list_server_name = Some(server_name);
        self.app_server_form_mode = ServerFormMode::Edit;
        self.app_server_form_step = 0;
        self.app_server_form_fields = vec![
            endpoint.name.clone(),
            endpoint.port.to_string(),
            if endpoint.secure {
                "y".to_string()
            } else {
                "n".to_string()
            },
            endpoint.content_database.clone().unwrap_or_default(),
            endpoint.modules_database.clone().unwrap_or_default(),
        ];
        self.app_server_edit_target = Some(endpoint.name.clone());
        self.app_server_edit_group = Some(endpoint.group.clone());
        self.mode = AppMode::AppServerForm;
    }

    fn toggle_selected_app_server_favorite(&mut self) {
        let Some((server_name, group_name, app_server_name)) = self.selected_app_server_key()
        else {
            self.status_message = "Select an app server to toggle a favorite.".to_string();
            return;
        };
        let selected_index = match self.servers_interface_focus {
            ServersInterfaceFocus::Favorites => self.servers_interface_favorites_state.selected(),
            ServersInterfaceFocus::Tree => self.servers_interface_tree_state.selected(),
        };

        let new_favorite = self
            .config
            .servers
            .iter_mut()
            .find(|server| server.name == server_name)
            .and_then(|server| {
                server.app_servers.iter_mut().find(|app_server| {
                    app_server.group == group_name && app_server.name == app_server_name
                })
            })
            .map(|app_server| {
                app_server.favorite = !app_server.favorite;
                app_server.favorite
            });
        let Some(new_favorite) = new_favorite else {
            self.status_message = "No app server selected.".to_string();
            return;
        };

        match self.save_app_config() {
            Ok(()) => {
                self.rebuild_servers_interface_rows();
                if self.servers_interface_focus == ServersInterfaceFocus::Favorites {
                    if self.servers_interface_favorites.is_empty() {
                        self.servers_interface_favorites_state.select(None);
                    } else {
                        let last = self.servers_interface_favorites.len().saturating_sub(1);
                        let restored = selected_index.map(|index| index.min(last));
                        self.servers_interface_favorites_state.select(restored);
                    }
                }
                self.status_message = if new_favorite {
                    format!(
                        "Favorited app server: {} / {}",
                        server_name, app_server_name
                    )
                } else {
                    format!(
                        "Removed app server from favorites: {} / {}",
                        server_name, app_server_name
                    )
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to save app server favorite: {}", e);
            }
        }
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

        let secure =
            secure_input.eq_ignore_ascii_case("y") || secure_input.eq_ignore_ascii_case("yes");

        let Some(server_name) = self.app_server_list_server_name.clone() else {
            self.status_message = "No server context.".to_string();
            return;
        };

        let was_edit = self.app_server_form_mode == ServerFormMode::Edit;
        let group = self
            .app_server_edit_group
            .clone()
            .unwrap_or_else(client::default_app_server_group);
        let old_favorite = if was_edit {
            self.app_server_edit_target
                .as_deref()
                .and_then(|old_name| {
                    self.config
                        .servers
                        .iter()
                        .find(|s| s.name == server_name.as_str())
                        .and_then(|server| {
                            server.app_servers.iter().find(|app_server| {
                                app_server.group == group && app_server.name == old_name
                            })
                        })
                })
                .map(|app_server| app_server.favorite)
        } else {
            None
        };

        let endpoint = AppServerEndpoint {
            name: name.clone(),
            port,
            secure,
            group: group.clone(),
            favorite: old_favorite.unwrap_or(false),
            content_database: if content_db.is_empty() {
                None
            } else {
                Some(content_db.to_string())
            },
            modules_database: if modules_db.is_empty() {
                None
            } else {
                Some(modules_db.to_string())
            },
        };

        if let Some(server) = self
            .config
            .servers
            .iter_mut()
            .find(|s| s.name == server_name.as_str())
        {
            server
                .app_servers
                .retain(|app_server| !(app_server.group == group && app_server.name == name));
            server.app_servers.push(endpoint);
            server.app_servers.sort_by(|a, b| {
                a.group
                    .to_lowercase()
                    .cmp(&b.group.to_lowercase())
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
        }

        if was_edit
            && self.config.active_server.as_deref() == Some(server_name.as_str())
            && let Some(old_name) = &self.app_server_edit_target
            && self.config.active_app_server.as_deref() == Some(old_name.as_str())
            && old_name != &name
        {
            self.config.active_app_server = Some(name.clone());
        }

        match self.save_app_config() {
            Ok(()) => {
                self.rebuild_servers_interface_rows();
                if was_edit {
                    let selected_row = ServersInterfaceRow::AppServer {
                        server_name: server_name.clone(),
                        group_name: group.clone(),
                        app_server_name: name.clone(),
                    };
                    if self.servers_interface_focus == ServersInterfaceFocus::Favorites {
                        if let Some(index) = self
                            .servers_interface_favorites
                            .iter()
                            .position(|row| *row == selected_row)
                        {
                            self.servers_interface_favorites_state.select(Some(index));
                        }
                    } else if let Some(index) = self
                        .servers_interface_tree
                        .iter()
                        .position(|row| *row == selected_row)
                    {
                        self.servers_interface_tree_state.select(Some(index));
                    }
                }
                self.app_server_edit_target = None;
                self.app_server_edit_group = None;
                self.mode = AppMode::Interface(AppInterface::Servers);
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

    fn start_delete_selected_app_server(&mut self) {
        let Some((server_name, group_name, app_server_name)) = self.selected_app_server_key()
        else {
            self.status_message = "No app server selected.".to_string();
            return;
        };
        self.app_server_list_server_name = Some(server_name);
        self.app_server_delete_group = Some(group_name);
        self.app_server_delete_target = Some(app_server_name);
        self.mode = AppMode::AppServerDeleteConfirm;
    }

    fn delete_app_server(&mut self) {
        let Some(server_name) = self.app_server_list_server_name.clone() else {
            return;
        };
        let Some(group_name) = self.app_server_delete_group.take() else {
            self.mode = AppMode::Interface(AppInterface::Servers);
            return;
        };
        let Some(endpoint_name) = self.app_server_delete_target.take() else {
            self.mode = AppMode::Interface(AppInterface::Servers);
            return;
        };

        let was_active_pair = self.config.active_server.as_deref() == Some(server_name.as_str())
            && self.config.active_app_server.as_deref() == Some(endpoint_name.as_str());
        if let Some(server) = self
            .config
            .servers
            .iter_mut()
            .find(|s| s.name == server_name)
        {
            server
                .app_servers
                .retain(|a| !(a.group == group_name && a.name == endpoint_name));
        }
        if was_active_pair {
            self.config.active_app_server = None;
            self.config.active_database = None;
            self.config.active_modules_database = None;
        }

        match self.save_app_config() {
            Ok(()) => {
                if was_active_pair {
                    self.reconnect();
                }
                self.rebuild_servers_interface_rows();
                self.mode = AppMode::Interface(AppInterface::Servers);
                self.status_message = format!("Removed app server: {}", endpoint_name);
            }
            Err(e) => {
                self.status_message = format!("Failed to remove app server: {}", e);
            }
        }
    }

    fn discover_app_servers_for_server(&self, server_name: &str) -> Result<Vec<AppServerEndpoint>> {
        let server_config = self
            .config
            .servers
            .iter()
            .find(|server| server.name == server_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("server '{}' is not configured", server_name))?;
        let client = MarkLogicClient::new(server_config)?;
        self.rt.block_on(client.list_app_servers())
    }

    fn refresh_app_servers_from_discovery(&mut self) {
        let Some(server_name) = self.app_server_list_server_name.clone() else {
            return;
        };
        match self.discover_app_servers_for_server(&server_name) {
            Ok(discovered) => {
                self.app_server_discovery_parent = Some(server_name.clone());
                self.app_server_discovery_results = discovered;
                self.app_server_discovery_marked = self
                    .config
                    .servers
                    .iter()
                    .find(|server| server.name == server_name)
                    .map(|server| {
                        server
                            .app_servers
                            .iter()
                            .map(|endpoint| Self::app_server_key(&endpoint.group, &endpoint.name))
                            .collect()
                    })
                    .unwrap_or_default();
                self.app_server_discovery_state
                    .select((!self.app_server_discovery_results.is_empty()).then_some(0));
                self.mode = AppMode::AppServerDiscovery;
                self.status_message = format!(
                    "Discovered {} app server(s) on '{}'. Space marks, Enter saves.",
                    self.app_server_discovery_results.len(),
                    server_name
                );
            }
            Err(e) => {
                self.status_message = format!("Failed to discover app servers: {}", e);
            }
        }
    }

    fn toggle_discovered_app_server_mark(&mut self) {
        let Some(index) = self.app_server_discovery_state.selected() else {
            self.status_message = "No discovered app server selected.".to_string();
            return;
        };
        let Some(endpoint) = self.app_server_discovery_results.get(index) else {
            self.status_message = "No discovered app server selected.".to_string();
            return;
        };
        let key = Self::app_server_key(&endpoint.group, &endpoint.name);
        if !self.app_server_discovery_marked.remove(&key) {
            self.app_server_discovery_marked.insert(key);
        }
    }

    fn set_all_discovered_app_servers_marked(&mut self, marked: bool) {
        self.app_server_discovery_marked = if marked {
            self.app_server_discovery_results
                .iter()
                .map(|endpoint| Self::app_server_key(&endpoint.group, &endpoint.name))
                .collect()
        } else {
            HashSet::new()
        };
    }

    fn save_selected_discovered_app_servers(&mut self) {
        let Some(server_name) = self.app_server_discovery_parent.clone() else {
            self.mode = AppMode::Interface(AppInterface::Servers);
            return;
        };
        if self.app_server_discovery_marked.is_empty() {
            self.status_message =
                "No app servers marked. Use Space to select remote app servers.".to_string();
            return;
        }

        let selected_results = self
            .app_server_discovery_results
            .iter()
            .filter(|endpoint| {
                self.app_server_discovery_marked
                    .contains(&Self::app_server_key(&endpoint.group, &endpoint.name))
            })
            .cloned()
            .collect::<Vec<_>>();
        let copied_count = selected_results.len();

        let Some(server) = self
            .config
            .servers
            .iter_mut()
            .find(|server| server.name == server_name)
        else {
            self.mode = AppMode::Interface(AppInterface::Servers);
            return;
        };
        for selected in selected_results {
            if let Some(existing) = server
                .app_servers
                .iter_mut()
                .find(|endpoint| endpoint.group == selected.group && endpoint.name == selected.name)
            {
                existing.port = selected.port;
                existing.secure = selected.secure;
                existing.content_database = selected.content_database;
                existing.modules_database = selected.modules_database;
            } else {
                server.app_servers.push(selected);
            }
        }
        server.app_servers.sort_by(|a, b| {
            a.group
                .to_lowercase()
                .cmp(&b.group.to_lowercase())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        match self.save_app_config() {
            Ok(()) => {
                self.expanded_servers.insert(server_name.clone());
                self.rebuild_servers_interface_rows();
                self.app_server_discovery_parent = None;
                self.app_server_discovery_results.clear();
                self.app_server_discovery_marked.clear();
                self.app_server_discovery_state.select(None);
                self.mode = AppMode::Interface(AppInterface::Servers);
                self.status_message = format!(
                    "Copied {} app server(s) from '{}'.",
                    copied_count, server_name
                );
            }
            Err(e) => {
                self.status_message = format!("Failed to save discovered app servers: {}", e);
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
        let Some(server_name) = self.selected_server_name() else {
            self.status_message = "No server selected.".to_string();
            return;
        };
        let Some(server) = self.config.servers.iter().find(|s| s.name == server_name) else {
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
            if server.insecure {
                "y".to_string()
            } else {
                "n".to_string()
            },
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
                    self.status_message =
                        "Auth type must be digest, basic, digestbasic, or application-level."
                            .to_string();
                    return;
                }
            }
        };

        let insecure =
            insecure_input.eq_ignore_ascii_case("y") || insecure_input.eq_ignore_ascii_case("yes");

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

        if let Some(old_name) = old_name.as_deref()
            && old_name != name
        {
            self.config.remove_server(old_name);
        }

        let app_servers = old_server
            .as_ref()
            .map(|s| s.app_servers.clone())
            .unwrap_or_else(|| vec![AppServerEndpoint::app_services_default()]);

        let (port, secure) = old_server
            .as_ref()
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

        match self.save_app_config() {
            Ok(()) => {
                self.reconnect();
                self.rebuild_servers_interface_rows();
                let selected_row = ServersInterfaceRow::Server {
                    server_name: name.clone(),
                };
                if let Some(index) = self
                    .servers_interface_tree
                    .iter()
                    .position(|row| *row == selected_row)
                {
                    self.servers_interface_tree_state.select(Some(index));
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
        self.expanded_servers.remove(&name);
        match self.save_app_config() {
            Ok(()) => {
                self.reconnect();
                self.rebuild_servers_interface_rows();
                self.status_message = format!("Removed server: {}", name);
            }
            Err(e) => {
                self.status_message = format!("Failed to remove server: {}", e);
            }
        }
        self.mode = AppMode::Interface(AppInterface::Servers);
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

    fn open_active_variables(&mut self) {
        self.active_variable_fields = self
            .config
            .active_variables
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        if self.active_variable_fields.is_empty() {
            self.active_variable_fields
                .push((String::new(), String::new()));
        }
        self.active_variable_step = 0;
        self.open_modal(AppMode::ActiveVariables);
    }

    fn save_active_variables(&mut self) {
        let mut variables = BTreeMap::new();
        for (name, value) in &self.active_variable_fields {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if !name.chars().enumerate().all(|(index, ch)| {
                ch == '_' || ch.is_ascii_alphanumeric() && (index > 0 || ch.is_ascii_alphabetic())
            }) {
                self.status_message = format!("Invalid active-variable name: {name}");
                return;
            }
            if variables.insert(name.to_string(), value.clone()).is_some() {
                self.status_message = format!("Duplicate active-variable name: {name}");
                return;
            }
        }
        self.config.active_variables = variables;
        if let Err(error) = self.save_app_config() {
            self.status_message = format!("Failed to save active variables: {error}");
            return;
        }
        self.status_message = format!(
            "Saved {} active variable(s).",
            self.config.active_variables.len()
        );
        self.close_modal_with_focus(Focus::Query);
    }

    fn open_declared_variables(&mut self) {
        self.declared_variable_fields = declared_variables(&self.query_text())
            .into_iter()
            .map(|variable| (variable.name, variable.value))
            .collect();
        if self.declared_variable_fields.is_empty() {
            self.status_message = "No declared XQuery variables found.".to_string();
            return;
        }
        self.declared_variable_step = 0;
        self.open_modal(AppMode::DeclaredVariables);
    }

    fn submit_declared_variables(&mut self) {
        let overrides = self
            .declared_variable_fields
            .iter()
            .filter(|(_, value)| !value.trim().is_empty())
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        self.close_modal_with_focus(Focus::Query);
        self.execute_query_with_declared_variables(overrides);
    }

    fn execute_query(&mut self) {
        self.execute_query_with_declared_variables(BTreeMap::new());
    }

    fn execute_query_with_declared_variables(&mut self, overrides: BTreeMap<String, String>) {
        let Some(path) = self.active_query_file.clone() else {
            self.results_text = "No query file selected.".to_string();
            return;
        };

        if self.running_queries.contains_key(&path) {
            self.show_query_busy();
            return;
        }

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
            let kind = query_execution_kind(&path);
            if !self.check_query_kind(&kind, &path) {
                return;
            }

            // Resolve `import function` / `import variable` directives before
            // the query leaves the process. The editor keeps the directives.
            let Some(query) = self.expand_query_imports(&kind, query, &path) else {
                return;
            };
            let query = substitute_active_variables(&query, &self.config.active_variables);
            let query = apply_declared_variable_overrides(&query, &overrides);

            // Run the request in the background so the UI stays responsive.
            // Completion is delivered through the query channel and applied
            // by `poll_query_completions` on the next event-loop iteration.
            let is_javascript = matches!(kind, QueryExecutionKind::JavaScript);
            let started_at = Instant::now();
            let tx = self.query_tx.clone();
            let handle = self.rt.spawn(async move {
                let result = if is_javascript {
                    client.js_query(&query).await
                } else {
                    client.xquery_query(&query).await
                };
                let outcome = result.map_err(format_query_error);
                let _ = tx.send(QueryEvent::Completion(QueryCompletion {
                    path,
                    started_at,
                    outcome,
                    total_runs: 1,
                }));
            });
            self.running_queries.insert(
                self.active_query_file.clone().expect("active query file"),
                RunningQuery {
                    started_at,
                    abort: handle.abort_handle(),
                },
            );
            self.clear_query_results_view();
            self.results_text = "Running query...".to_string();
            // Give the results panel focus (and the larger split) for the run.
            self.focus_results_panel();
        } else {
            warn!("execute_query requested without active client");
            self.results_text = "No server connected.".to_string();
        }
    }

    /// Elapsed time since the query for `path` started, if it is still running.
    fn running_query_elapsed(&self, path: &Path) -> Option<Duration> {
        self.running_queries
            .get(path)
            .map(|running| running.started_at.elapsed())
    }

    /// Show the busy state for the active query file: clear the results pane
    /// and tell the user the query is still running.
    fn show_query_busy(&mut self) {
        self.clear_query_results_view();
        self.results_text = "Busy, check back later".to_string();
    }

    /// Cancel the running query for the active query file, if any.
    fn cancel_running_query(&mut self) {
        let Some(path) = self.active_query_file.clone() else {
            self.status_message = "No query file selected.".to_string();
            return;
        };
        if let Some(running) = self.running_queries.remove(&path) {
            running.abort.abort();
            self.running_partial_results.remove(&path);
            self.results_text = "Query cancelled.".to_string();
            self.status_message = format!(
                "Cancelled running query: {}",
                display_query_path(&path, &self.query_root_dir)
            );
        } else {
            self.status_message = "No running query for this file.".to_string();
        }
    }

    /// Reject non-executable query kinds, showing the reason in the results pane.
    fn check_query_kind(&mut self, kind: &QueryExecutionKind, path: &Path) -> bool {
        match kind {
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
                false
            }
            QueryExecutionKind::Unsupported => {
                self.query_results.clear();
                self.results_text = format!(
                    "Unsupported query file type: {}",
                    display_query_path(path, &self.query_root_dir)
                );
                false
            }
            QueryExecutionKind::JavaScript | QueryExecutionKind::XQuery => true,
        }
    }

    /// Expand XQuery `import` directives; on failure show the error and return None.
    fn expand_query_imports(
        &mut self,
        kind: &QueryExecutionKind,
        query: String,
        path: &Path,
    ) -> Option<String> {
        if !matches!(kind, QueryExecutionKind::XQuery) {
            return Some(query);
        }
        match xquery_imports::expand_xquery_imports(&query, path) {
            Ok(expanded) => Some(expanded),
            Err(e) => {
                self.query_results.clear();
                self.results_text = format!(
                    "Import error in {}: {:#}",
                    display_query_path(path, &self.query_root_dir),
                    e
                );
                self.status_message = "Query not run: import failed.".to_string();
                None
            }
        }
    }

    /// Open the multi-run form for the active query file.
    fn open_multi_run(&mut self) {
        let Some(path) = self.active_query_file.clone() else {
            self.status_message = "No query file selected.".to_string();
            return;
        };
        if self.running_queries.contains_key(&path) {
            self.show_query_busy();
            return;
        }
        self.multi_run_form_fields = vec![
            self.last_multi_run_count.to_string(),
            self.last_multi_run_wait_secs.to_string(),
        ];
        self.multi_run_form_step = 0;
        self.open_modal(AppMode::MultiRunForm);
    }

    /// Validate the multi-run form and start the background multi-run.
    fn submit_multi_run_form(&mut self) {
        let count_input = self
            .multi_run_form_fields
            .first()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let wait_input = self
            .multi_run_form_fields
            .get(1)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let count: u32 = match count_input.parse() {
            Ok(n) if (1..=MAX_MULTI_RUN_COUNT).contains(&n) => n,
            _ => {
                self.status_message = format!(
                    "Runs must be a whole number from 1 to {}.",
                    MAX_MULTI_RUN_COUNT
                );
                return;
            }
        };
        let wait_secs: f64 = match wait_input.parse::<f64>() {
            Ok(w) if w >= 0.0 && w.is_finite() => w,
            _ => {
                self.status_message = "Wait must be a number of seconds, 0 or more.".to_string();
                return;
            }
        };

        self.last_multi_run_count = count;
        self.last_multi_run_wait_secs = wait_secs;
        self.close_modal_restore_focus();
        self.execute_multi_run(count, wait_secs);
    }

    /// Run the active query `count` times in the background, concatenating
    /// results newest-first with separators. The UI stays responsive.
    fn execute_multi_run(&mut self, count: u32, wait_secs: f64) {
        let Some(path) = self.active_query_file.clone() else {
            self.results_text = "No query file selected.".to_string();
            return;
        };

        if self.running_queries.contains_key(&path) {
            self.show_query_busy();
            return;
        }

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
                count,
                wait_secs,
                "executing multi-run query"
            );
            let kind = query_execution_kind(&path);
            if !self.check_query_kind(&kind, &path) {
                return;
            }
            let Some(query) = self.expand_query_imports(&kind, query, &path) else {
                return;
            };
            let query = substitute_active_variables(&query, &self.config.active_variables);

            let is_javascript = matches!(kind, QueryExecutionKind::JavaScript);
            let total = count as usize;
            let started_at = Instant::now();
            let tx = self.query_tx.clone();
            let handle = self.rt.spawn(async move {
                let mut blocks: Vec<(usize, Vec<String>, Option<String>)> =
                    Vec::with_capacity(total);
                for run in 1..=total {
                    if run > 1 && wait_secs > 0.0 {
                        tokio::time::sleep(Duration::from_secs_f64(wait_secs)).await;
                    }
                    let result = if is_javascript {
                        client.js_query(&query).await
                    } else {
                        client.xquery_query(&query).await
                    };
                    match result {
                        Ok(parts) => {
                            blocks.push((run, parts, None));
                            // Stream the cumulative results so the pane
                            // updates between runs.
                            let _ = tx.send(QueryEvent::Progress(QueryProgress {
                                path: path.clone(),
                                run,
                                total,
                                cumulative: assemble_multi_run_results(&blocks),
                            }));
                        }
                        Err(e) => {
                            blocks.push((run, Vec::new(), Some(format_query_error(e))));
                            break;
                        }
                    }
                }
                let combined = assemble_multi_run_results(&blocks);
                let _ = tx.send(QueryEvent::Completion(QueryCompletion {
                    path,
                    started_at,
                    outcome: Ok(combined),
                    total_runs: total,
                }));
            });
            self.running_queries.insert(
                self.active_query_file.clone().expect("active query file"),
                RunningQuery {
                    started_at,
                    abort: handle.abort_handle(),
                },
            );
            self.clear_query_results_view();
            self.results_text = format!("Running multi-run ({} runs)...", count);
            // Give the results panel focus (and the larger split) for the run.
            self.focus_results_panel();
            self.status_message = format!("Multi-run started: {} runs", count);
        } else {
            warn!("execute_multi_run requested without active client");
            self.results_text = "No server connected.".to_string();
        }
    }

    /// Drain background query events (progress + completions) and apply them.
    fn poll_query_completions(&mut self) {
        while let Ok(event) = self.query_rx.try_recv() {
            match event {
                QueryEvent::Progress(progress) => self.apply_query_progress(progress),
                QueryEvent::Completion(completion) => self.apply_query_completion(completion),
            }
        }
    }

    fn apply_query_progress(&mut self, progress: QueryProgress) {
        let QueryProgress {
            path,
            run,
            total,
            cumulative,
        } = progress;
        // Ignore stale progress for a run that is no longer tracked
        // (e.g. cancelled between send and poll).
        if !self.running_queries.contains_key(&path) {
            return;
        }
        self.status_message = format!(
            "Multi-run {}/{}: {}",
            run,
            total,
            display_query_path(&path, &self.query_root_dir)
        );
        self.running_partial_results
            .insert(path.clone(), cumulative.clone());
        if self.active_query_file.as_ref() == Some(&path) {
            // Show the cumulative results live, keeping the running
            // indicator (no completion timestamp yet).
            self.set_query_results(cumulative);
            self.query_results_timestamp = None;
        }
    }

    fn apply_query_completion(&mut self, completion: QueryCompletion) {
        let QueryCompletion {
            path,
            started_at,
            outcome,
            total_runs,
        } = completion;
        self.running_queries.remove(&path);
        self.running_partial_results.remove(&path);
        let elapsed_secs = started_at.elapsed().as_secs();
        let display_name = display_query_path(&path, &self.query_root_dir);
        let is_active = self.active_query_file.as_ref() == Some(&path);

        match outcome {
            Ok(parts) => {
                info!(
                    file = %display_name,
                    parts = parts.len(),
                    elapsed_secs,
                    "background query completed"
                );
                if is_active {
                    let no_output = parts.is_empty();
                    self.set_query_results(parts);
                    if no_output {
                        self.results_text = NO_OUTPUT_MESSAGE.to_string();
                    }
                    self.persist_query_results_for_path(&path);
                } else {
                    let snapshot = QueryResultSnapshot {
                        query_results: parts.clone(),
                        selected_index: (!parts.is_empty()).then_some(0),
                        created_at: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs()),
                    };
                    if let Err(e) = save_query_result_snapshot(
                        &self.query_result_cache_dir,
                        &self.query_root_dir,
                        &path,
                        &snapshot,
                    ) {
                        self.status_message = format!(
                            "Query finished for {} but failed to cache: {}",
                            display_name, e
                        );
                    } else if total_runs > 1 {
                        self.set_transient_status_message(
                            format!(
                                "Multi-run finished: {} ({} runs, {}s)",
                                display_name, total_runs, elapsed_secs
                            ),
                            Duration::from_secs(8),
                        );
                    } else {
                        self.set_transient_status_message(
                            format!(
                                "Query finished: {} ({} results, {}s)",
                                display_name,
                                parts.len(),
                                elapsed_secs
                            ),
                            Duration::from_secs(8),
                        );
                    }
                }
            }
            Err(error_text) => {
                error!(
                    file = %display_name,
                    elapsed_secs,
                    "background query failed"
                );
                if is_active {
                    self.query_results.clear();
                    self.query_results_state.select(None);
                    self.results_text = error_text;
                } else {
                    self.set_transient_status_message(
                        format!("Query failed: {} ({}s)", display_name, elapsed_secs),
                        Duration::from_secs(8),
                    );
                }
            }
        }
    }

    fn delete_selected(&mut self) {
        if self.selected_indices.is_empty() {
            self.status_message = "No records selected for deletion.".to_string();
            return;
        }
        self.open_modal(AppMode::DeleteConfirm);
    }

    fn delete_selected_uri_query_result(&mut self) {
        let Some(selected) = self.query_results_state.selected() else {
            self.status_message = "No URI selected for deletion.".to_string();
            return;
        };
        let Some(result) = self.uri_query_results.get(selected) else {
            self.status_message = "No URI selected for deletion.".to_string();
            return;
        };
        if result.uri.is_empty() {
            self.status_message = "Select a non-empty URI first.".to_string();
            return;
        }
        self.query_uri_delete_target = Some(result.uri.clone());
        self.open_modal(AppMode::DeleteConfirm);
    }

    fn confirm_delete(&mut self) {
        let uris = self.delete_uris();
        let deleted_query_uri = self.query_uri_delete_target.clone();
        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.delete_documents(&uris)) {
                Ok(()) => {
                    self.status_message = format!("Deleted {} documents.", uris.len());
                    if let Some(uri) = deleted_query_uri {
                        self.remove_deleted_uri_query_result(&uri);
                    } else {
                        self.selected_indices.clear();
                        self.fetch_list();
                    }
                }
                Err(e) => {
                    self.status_message = format!("Delete error: {}", e);
                }
            }
        } else {
            self.status_message = "No server connected.".to_string();
        }
        self.query_uri_delete_target = None;
        self.close_modal_with_focus(Focus::Results);
    }

    fn delete_uris(&self) -> Vec<String> {
        if let Some(uri) = self.query_uri_delete_target.as_ref() {
            return vec![uri.clone()];
        }
        self.selected_indices
            .iter()
            .filter_map(|&i| self.records.get(i).map(|r| r.uri.clone()))
            .collect()
    }

    fn remove_deleted_uri_query_result(&mut self, uri: &str) {
        let selected = self.query_results_state.selected();
        let raw_index = selected
            .and_then(|index| self.visible_query_result_indices().get(index).copied())
            .or_else(|| {
                self.query_results
                    .iter()
                    .position(|result| result.trim() == uri)
            });
        let Some(raw_index) = raw_index else {
            return;
        };
        self.query_results.remove(raw_index);
        if let Some(index) = selected.filter(|index| *index < self.uri_query_results.len()) {
            self.uri_query_results.remove(index);
        }
        let next_selected = self
            .visible_query_result_count()
            .checked_sub(1)
            .map(|last| selected.unwrap_or(0).min(last));
        self.query_results_state.select(next_selected);
        self.query_results_timestamp = Some(SystemTime::now());
        if let Some(path) = self.active_query_file.clone() {
            self.persist_query_results_for_path(&path);
        }
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

fn format_query_error(e: anyhow::Error) -> String {
    let mut causes = e.chain();
    let mut error_text = format!("Query error: {}", causes.next().expect("error has a cause"));
    for cause in causes {
        error_text.push_str(&format!("\nCaused by: {cause}"));
    }
    query_error::format_message(&error_text)
}

fn substitute_active_variables(query: &str, variables: &BTreeMap<String, String>) -> String {
    variables
        .iter()
        .fold(query.to_string(), |query, (name, value)| {
            query.replace(&format!("%%{name}%%"), value)
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredVariable {
    name: String,
    value: String,
    value_range: std::ops::Range<usize>,
}

/// Extract simple XQuery prolog variable declarations. The replacement range
/// covers the default expression after `:=`.
fn declared_variables(query: &str) -> Vec<DeclaredVariable> {
    let mut variables = Vec::new();
    let mut search_from = 0;
    while let Some(found) = query[search_from..].find("declare variable") {
        let start = search_from + found;
        let Some(end) = xquery_statement_end(query, start) else {
            break;
        };
        let declaration = &query[start..=end];
        let Some(dollar) = declaration.find('$') else {
            search_from = end + 1;
            continue;
        };
        let name_start = dollar + 1;
        let name_len = declaration[name_start..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':'))
            .map(char::len_utf8)
            .sum::<usize>();
        if name_len == 0 {
            search_from = end + 1;
            continue;
        }
        let name = declaration[name_start..name_start + name_len].to_string();
        let after_name = name_start + name_len;
        let (replacement, value) = if let Some(assign) = declaration[after_name..].find(":=") {
            let value_start = start + after_name + assign + 2;
            (value_start..end, query[value_start..end].trim().to_string())
        } else {
            search_from = end + 1;
            continue;
        };
        variables.push(DeclaredVariable {
            name,
            value,
            value_range: replacement,
        });
        search_from = end + 1;
    }
    variables
}

fn xquery_statement_end(query: &str, start: usize) -> Option<usize> {
    let bytes = query.as_bytes();
    let mut quote = None;
    let mut index = start;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(current_quote) = quote {
            if byte == current_quote {
                // XQuery escapes quotes by doubling them.
                if bytes.get(index + 1) == Some(&current_quote) {
                    index += 2;
                    continue;
                }
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
        } else if byte == b';' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn apply_declared_variable_overrides(query: &str, overrides: &BTreeMap<String, String>) -> String {
    let mut query = query.to_string();
    for variable in declared_variables(&query).into_iter().rev() {
        if let Some(value) = overrides.get(&variable.name) {
            query.replace_range(variable.value_range, value);
        }
    }
    query
}

/// Match a result against a case-insensitive glob. `*` matches any sequence
/// and `?` matches one character; a pattern without wildcard characters is a
/// substring match so a quick `/term` filter remains useful.
fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    if !pattern.iter().any(|ch| matches!(ch, '*' | '?')) {
        let pattern = pattern.iter().collect::<String>().to_ascii_lowercase();
        return value
            .iter()
            .collect::<String>()
            .to_ascii_lowercase()
            .contains(&pattern);
    }

    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star, mut star_value_index) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?'
                || pattern[pattern_index].eq_ignore_ascii_case(&value[value_index]))
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// Assemble multi-run blocks — chronological `(run_no, parts, error)` tuples —
/// into display parts, newest run first. A `--- MULTI-RUN #<i> ---` separator
/// heads each older block (a `FAILED` variant when that run errored).
fn assemble_multi_run_results(blocks: &[(usize, Vec<String>, Option<String>)]) -> Vec<String> {
    let mut combined = Vec::new();
    let mut newest = true;
    for (run_no, parts, error) in blocks.iter().rev() {
        if !newest || error.is_some() {
            if error.is_some() {
                combined.push(format!("--- MULTI-RUN #{} FAILED ---", run_no));
            } else {
                combined.push(format!("--- MULTI-RUN #{} ---", run_no));
            }
        }
        newest = false;
        combined.extend(parts.iter().cloned());
        if let Some(message) = error {
            combined.push(message.clone());
        }
    }
    combined
}

/// Remove empty folders under the query root (and their cache mirrors), then
/// list the query tree. Folders only persist while they contain something.
fn load_query_tree(query_root_dir: &Path, cache_root_dir: &Path) -> Result<QueryTree> {
    for folder in prune_empty_query_folders(query_root_dir) {
        let _ = remove_query_result_cache(cache_root_dir, query_root_dir, &folder);
    }
    let mut tree = discover_query_tree(query_root_dir)?;
    if is_default_query_root(query_root_dir) {
        // The default store has no project files to exclude, so top-level
        // files are shown like any other.
        tree.files.append(&mut tree.loose_files);
        tree.files.sort_by_key(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
        });
    }
    Ok(tree)
}

/// Move every key equal to or under `old` to the same location under `new`.
fn remap_map_keys<V>(map: &mut HashMap<PathBuf, V>, old: &Path, new: &Path) {
    let moved: Vec<PathBuf> = map
        .keys()
        .filter(|key| key.starts_with(old))
        .cloned()
        .collect();
    for key in moved {
        if let (Some(value), Some(new_key)) = (map.remove(&key), remap_path_prefix(&key, old, new))
        {
            map.insert(new_key, value);
        }
    }
}

fn format_sync_comment(db: &str, uri: &str, is_xquery: bool) -> String {
    if is_xquery {
        format!("(:!%sync%|DB[{}]|URI[{}] !:)", db, uri)
    } else {
        format!("/* !%sync%|DB[{}]|URI[{}] ! */", db, uri)
    }
}

fn parse_sync_comment(content: &str) -> Option<SyncInfo> {
    let first_line = content.lines().next()?;
    let trimmed = first_line.trim();
    if let Some(rest) = trimmed.strip_prefix("(:!%sync%|") {
        let rest = rest.strip_suffix(" !:)")?;
        return parse_sync_body(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("/* !%sync%|") {
        let rest = rest.strip_suffix(" ! */")?;
        return parse_sync_body(rest);
    }
    None
}

fn parse_sync_body(body: &str) -> Option<SyncInfo> {
    let db_start = body.find("DB[")? + 3;
    let db_end = body[db_start..].find(']')? + db_start;
    let db = body[db_start..db_end].to_string();
    let uri_start = body.find("URI[")? + 4;
    let uri_end = body[uri_start..].find(']')? + uri_start;
    let uri = body[uri_start..uri_end].to_string();
    Some(SyncInfo { db, uri })
}

fn strip_sync_comment(content: &str) -> String {
    let mut removed = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        if !removed && parse_sync_comment(line).is_some() {
            removed = true;
        } else {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn is_xquery_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("xqy"))
        .unwrap_or(false)
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
        App, AppInterface, AppMode, EditMode, Focus, LOG_FILE_PATH, ModuleSearchState,
        NO_OUTPUT_MESSAGE, QueryCompletion, QueryEvent, QueryProgress, QueryResultSnapshot,
        QueryResultsFormat, RunningQuery, Screen, ServerFormMode, ServersInterfaceFocus,
        ServersInterfaceRow, SyncInfo, TrackedFolderEntry, TrackedFolderMenuItem,
        apply_declared_variable_overrides, assemble_multi_run_results, declared_variables,
        display_folder_path, format_query_error, format_sync_comment, handle_command_key,
        handle_filter_key, handle_fullscreen_key, handle_key_event, handle_log_viewer_key,
        handle_multi_run_form_key, handle_navigation_mode_key, handle_query_insert_transition_key,
        handle_query_key, handle_results_key, handle_return_to_start_page_key,
        handle_server_delete_confirm_key, handle_servers_interface_key, inline_editor_uses_edtui,
        is_xquery_file, map_query_navigation_key, parse_modules_database_override,
        parse_sync_comment, resolve_folder_input, save_query_result_snapshot, strip_sync_comment,
        substitute_active_variables, temp_editor_path, ui, wildcard_matches,
    };
    use crate::client::{AppServerEndpoint, AuthType, MarkLogicClient, ServerConfig};
    use crate::config::{AppConfig, ConfigBundle};
    use crate::tracked_folder::TrackedFolderStore;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use ratatui::widgets::{ListState, TableState};
    use std::{
        collections::{BTreeMap, HashMap, HashSet},
        fs,
        path::{Path, PathBuf},
        time::{Duration, Instant, SystemTime},
    };
    use tokio::runtime::Runtime;

    pub(crate) fn test_app() -> App {
        let (query_tx, query_rx) = tokio::sync::mpsc::unbounded_channel();
        // Give every test its own empty query root. Refresh prunes empty
        // folders, so a shared "." root would modify the working tree.
        let query_root = temp_test_dir("app-default-root");
        fs::create_dir_all(&query_root).unwrap();
        App {
            config: AppConfig::default(),
            client: None,
            focus: Focus::Results,
            terminal_focused: true,
            edit_mode: EditMode::Navigate,
            mode: AppMode::Normal,
            previous_mode: None,
            previous_screen: None,
            screen_states: HashMap::new(),
            help_scroll: 0,
            modal_origin_focus: None,
            interface_origin_focus: None,
            command_input: String::new(),
            query_editor: App::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            should_quit: false,
            query_root_dir: query_root,
            query_result_cache_dir: PathBuf::from(".marklogic-tui"),
            query_files: Vec::new(),
            query_folders: Vec::new(),
            loose_query_files: Vec::new(),
            collapsed_query_folders: HashSet::new(),
            active_query_file: None,
            query_file_list_state: ListState::default(),
            module_search: ModuleSearchState {
                pattern: String::new(),
                results: Vec::new(),
                selected: ListState::default(),
                page: 0,
                page_size: 50,
                total: None,
                focus_input: true,
                marked: Vec::new(),
            },
            module_drop_needs_reset: false,
            modules_view: false,
            cached_modules: Vec::new(),
            cached_modules_state: ListState::default(),
            collapsed_module_folders: HashSet::new(),
            module_diff_uri: None,
            module_diff_lines: Vec::new(),
            module_diff_scroll: 0,
            module_drop_target: None,
            new_module_uri_input: String::new(),
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
            multi_run_form_fields: vec![String::new(); 2],
            multi_run_form_step: 0,
            last_multi_run_count: 5,
            last_multi_run_wait_secs: 0.0,
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
            query_results_format: QueryResultsFormat::Plain,
            query_results_format_picker_state: ListState::default(),
            query_results_filter: None,
            uri_query_results: Vec::new(),
            query_uri_delete_target: None,
            running_queries: HashMap::new(),
            running_partial_results: HashMap::new(),
            query_tx,
            query_rx,
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
            mouse_regions: Vec::new(),
            environment_area: Rect::default(),
            server_picker_open: false,
            server_picker_error: None,
            server_picker_area: Rect::default(),
            server_picker_state: ListState::default(),
            last_query_results_g: None,
            active_document: None,
            full_view_content: String::new(),
            full_view_scroll: 0,
            server_logs: crate::logs::LogViewer::default(),
            reports: crate::reports_ui::Reports::default(),
            system: crate::system::System::default(),
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
            servers_interface_focus: ServersInterfaceFocus::Tree,
            servers_interface_favorites: Vec::new(),
            servers_interface_tree: Vec::new(),
            servers_interface_favorites_state: ListState::default(),
            servers_interface_tree_state: TableState::default(),
            expanded_servers: HashSet::new(),
            app_server_form_mode: ServerFormMode::Add,
            app_server_form_step: 0,
            app_server_form_fields: vec![String::new(); 5],
            app_server_edit_target: None,
            app_server_edit_group: None,
            app_server_delete_target: None,
            app_server_delete_group: None,
            app_server_list_server_name: None,
            app_server_discovery_parent: None,
            app_server_discovery_results: Vec::new(),
            app_server_discovery_state: TableState::default(),
            app_server_discovery_marked: HashSet::new(),
            doc_form_step: 0,
            doc_form_uri: String::new(),
            doc_form_collections: String::new(),
            doc_form_quality: "0".to_string(),
            doc_form_content: String::new(),
            active_variable_fields: Vec::new(),
            active_variable_step: 0,
            declared_variable_fields: Vec::new(),
            declared_variable_step: 0,
            config_bundle_path: None,
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
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
            group: crate::client::default_app_server_group(),
            content_database: Some("Documents".to_string()),
            modules_database: Some("Modules".to_string()),
            favorite: false,
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
    fn escape_resets_the_log_screen_and_double_escape_returns_to_landing() {
        let mut app = test_app();
        app.mode = AppMode::MarkLogicLogs;
        app.server_logs.file_filter = "error".into();
        app.server_logs.search = "warning".into();
        app.server_logs.show_untracked = true;
        app.server_logs.discovery_days_ago = 2;
        app.server_logs.files_focus = false;
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        assert!(app.server_logs.file_filter.is_empty());
        assert!(app.server_logs.search.is_empty());
        assert!(!app.server_logs.show_untracked);
        assert_eq!(app.server_logs.discovery_days_ago, 0);
        assert!(app.server_logs.files_focus);
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.mode, AppMode::StartPage);
    }

    #[test]
    fn double_escape_works_from_insert_mode_and_after_dismissing_a_dialog() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let mut app = test_app();
        app.focus_query_panel();
        app.enter_insert_mode();
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert_eq!(app.mode, AppMode::Normal);
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.mode, AppMode::StartPage);

        app.mode = AppMode::Normal;
        app.open_help_overlay();
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.mode, AppMode::Normal);
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.mode, AppMode::StartPage);
    }

    #[test]
    fn escape_sequence_requires_two_quick_consecutive_presses() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.full_view_scroll = 10;
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.full_view_scroll, 0);
        assert_eq!(app.mode, AppMode::FullScreenView);
        app.last_esc = Some(Instant::now() - Duration::from_secs(1));
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.mode, AppMode::FullScreenView);
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)).unwrap();
        handle_key_event(&mut app, esc).unwrap();
        assert_eq!(app.mode, AppMode::FullScreenView);
    }

    #[test]
    fn environment_indicator_and_mouse_targets_select_server_and_panels() {
        use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = test_app();
        app.config.servers = vec![
            test_server("one", "localhost"),
            test_server("two", "localhost"),
        ];
        app.config.active_server = Some("one".into());
        app.focus_query_panel();
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let click = |column, row| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let environment = app.environment_area;
        assert_eq!(environment.x, 0);
        assert!(environment.width > 0);
        crate::events::handle_terminal_event(&mut app, click(environment.x, environment.y))
            .unwrap();
        assert!(app.server_picker_open);
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let picker = app.server_picker_area;
        crate::events::handle_terminal_event(&mut app, click(picker.x + 1, picker.y + 2)).unwrap();
        assert_eq!(app.config.active_server.as_deref(), Some("two"));
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.server_picker_open);

        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let results = app
            .mouse_regions
            .iter()
            .find(|(_, target)| matches!(target, super::MouseTarget::Results))
            .unwrap()
            .0;
        crate::events::handle_terminal_event(&mut app, click(results.x + 1, results.y)).unwrap();
        assert_eq!(app.focus, Focus::Results);
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let query = app
            .mouse_regions
            .iter()
            .find(|(_, target)| matches!(target, super::MouseTarget::Query))
            .unwrap()
            .0;
        crate::events::handle_terminal_event(&mut app, click(query.x + 1, query.y)).unwrap();
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn server_picker_keeps_the_current_server_if_the_query_cannot_be_saved() {
        let mut app = test_app();
        app.config.servers = vec![
            test_server("one", "localhost"),
            test_server("two", "localhost"),
        ];
        app.config.active_server = Some("one".into());
        app.active_query_file = Some(app.query_root_dir.join("missing").join("query.xqy"));
        app.query_dirty = true;
        app.open_server_picker();
        app.server_picker_state.select(Some(1));
        app.activate_picker_server();
        assert_eq!(app.config.active_server.as_deref(), Some("one"));
        assert!(app.query_dirty);
        assert!(app.server_picker_open);
        assert!(app.server_picker_error.is_some());
        assert!(app.status_message.contains("Could not switch server"));
    }

    #[test]
    fn environment_indicator_is_on_every_screen_except_landing() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = test_app();
        app.config.servers = vec![test_server("staging", "localhost")];
        app.config.active_server = Some("staging".into());
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        for mode in [
            AppMode::Normal,
            AppMode::MarkLogicLogs,
            AppMode::FullScreenView,
            AppMode::System,
            AppMode::Interface(AppInterface::Servers),
            AppMode::ServerForm,
            AppMode::HelpOverlay,
        ] {
            app.mode = mode.clone();
            terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
            let row: String = (0..30)
                .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                .collect();
            assert!(row.starts_with("staging:8000 ▾"), "{mode:?}: {row}");
        }
        app.mode = AppMode::StartPage;
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        assert_eq!(app.environment_area.width, 0);
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
    fn q_in_fullscreen_view_does_not_return_to_start_page() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.focus_results_panel();

        assert!(
            !handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::FullScreenView);
    }

    #[test]
    fn q_in_servers_interface_does_not_return_to_start_page() {
        let mut app = test_app();
        app.focus_query_panel();
        app.open_servers_interface();

        assert!(
            !handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
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
        fs::create_dir_all(folder_two.join("q")).unwrap();
        fs::write(
            folder_two.join("q/query-2.xqy"),
            "xquery version \"1.0-ml\";",
        )
        .unwrap();
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
            vec![app.query_root_dir.join("q/query-2.xqy")]
        );
        assert_eq!(
            app.active_query_file,
            Some(app.query_root_dir.join("q/query-2.xqy"))
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

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn servers_interface_q_returns_to_normal_mode() {
        let mut app = test_app();
        app.focus_query_panel();
        app.open_servers_interface();

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
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
        assert_eq!(app.servers_interface_tree_state.selected(), None);
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
        assert_eq!(app.servers_interface_tree_state.selected(), Some(2));

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.servers_interface_tree_state.selected(), Some(0));
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
        let bundle_path = temp_test_dir("server-add-bundle").join("config.yaml");
        app.config_bundle_path = Some(bundle_path.clone());
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

        let yaml = fs::read_to_string(&bundle_path).expect("read config bundle");
        let bundle: ConfigBundle = serde_yaml::from_str(&yaml).expect("parse config bundle");
        assert_eq!(bundle.config.servers.len(), 1);
        assert_eq!(bundle.config.servers[0].name, "prod");
        assert_eq!(bundle.config.servers[0].host, "prod.example");
    }

    #[test]
    fn tracked_folder_changes_update_config_bundle() {
        let mut app = test_app();
        let bundle_path = temp_test_dir("tracked-folder-bundle").join("config.yaml");
        let folder = temp_test_dir("tracked-folder");
        fs::create_dir_all(&folder).unwrap();
        app.config_bundle_path = Some(bundle_path.clone());
        let canonical_folder = app.tracked_folders.add_folder(&folder).unwrap();
        app.tracked_folders.toggle_favorite(&canonical_folder);

        app.save_tracked_folders().unwrap();

        let yaml = fs::read_to_string(&bundle_path).expect("read config bundle");
        let bundle: ConfigBundle = serde_yaml::from_str(&yaml).expect("parse config bundle");
        assert_eq!(bundle.tracked_folders.folders.len(), 1);
        assert!(bundle.tracked_folders.folders[0].favorite);
        assert_eq!(
            bundle.tracked_folders.folders[0].path,
            canonical_folder.to_string_lossy()
        );
    }

    #[test]
    fn app_config_save_does_not_create_bundle_without_config_flag() {
        let app = test_app();
        let bundle_path = temp_test_dir("no-config-bundle").join("config.yaml");

        app.save_app_config().unwrap();

        assert!(!bundle_path.exists());
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
        assert_eq!(
            app.config.active_app_server.as_deref(),
            Some("App-Services")
        );
        assert_eq!(app.config.active_database.as_deref(), Some("Documents"));
        assert_eq!(
            app.config.active_modules_database.as_deref(),
            Some("Modules")
        );
    }

    #[test]
    fn server_add_rebuilds_servers_tree() {
        let mut app = test_app();
        app.open_server_add();
        app.server_form_fields[0] = "local".to_string();
        app.server_form_fields[1] = "localhost".to_string();
        app.server_form_fields[2] = "admin".to_string();
        app.server_form_fields[3] = "admin".to_string();
        app.server_form_fields[4] = "digest".to_string();
        app.server_form_fields[5] = "n".to_string();

        app.submit_server_form();

        assert_eq!(app.servers_interface_tree.len(), 1);
        assert_eq!(
            app.servers_interface_tree[0],
            ServersInterfaceRow::Server {
                server_name: "local".to_string(),
            }
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
                group: crate::client::default_app_server_group(),
                content_database: Some("Content".to_string()),
                modules_database: Some("Modules".to_string()),
                favorite: true,
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
        assert!(server.app_servers[0].favorite);
    }

    #[test]
    fn server_rename_preserves_app_servers_and_favorites() {
        let mut app = test_app();
        let mut server = test_server("old", "localhost");
        server.app_servers = vec![AppServerEndpoint::app_services_default()];
        server.app_servers[0].favorite = true;
        app.config.servers = vec![server];
        app.config.active_server = Some("old".to_string());
        app.open_servers_interface();
        app.open_server_edit();
        app.server_form_fields[0] = "new".to_string();
        app.submit_server_form();

        assert_eq!(app.config.servers.len(), 1);
        assert_eq!(app.config.servers[0].name, "new");
        assert_eq!(app.config.servers[0].app_servers.len(), 1);
        assert!(app.config.servers[0].app_servers[0].favorite);
        assert_eq!(app.config.active_server.as_deref(), Some("new"));
    }

    #[test]
    fn servers_interface_tab_cycles_favorites_and_tree() {
        let mut app = test_app();
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.servers_interface_focus,
            ServersInterfaceFocus::Favorites
        );

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.servers_interface_focus, ServersInterfaceFocus::Tree);
    }

    #[test]
    fn servers_interface_app_server_enter_sets_port_and_databases() {
        let mut app = test_app();
        let mut server = test_server("local", "localhost");
        server.app_servers = vec![AppServerEndpoint {
            name: "my-app-server".to_string(),
            port: 8010,
            secure: false,
            group: crate::client::default_app_server_group(),
            content_database: Some("Content-DB".to_string()),
            modules_database: Some("Modules-DB".to_string()),
            favorite: false,
        }];
        app.config.servers = vec![server];
        app.config.active_server = Some("local".to_string());
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.servers_interface_tree.len(), 3);
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(
            app.config.active_app_server.as_deref(),
            Some("my-app-server")
        );
        assert_eq!(app.config.active_database.as_deref(), Some("Content-DB"));
        assert_eq!(
            app.config.active_modules_database.as_deref(),
            Some("Modules-DB")
        );
        assert_eq!(app.config.servers[0].port, 8010);
    }

    #[test]
    fn servers_interface_enter_folds_and_opens_servers() {
        let mut app = test_app();
        let mut server = test_server("local", "localhost");
        server.app_servers = vec![AppServerEndpoint::app_services_default()];
        app.config.servers = vec![server];
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.servers_interface_tree.len(), 3);
        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.servers_interface_tree.len(), 1);
        assert!(!app.expanded_servers.contains("local"));
        assert_eq!(app.servers_interface_tree_state.selected(), Some(0));
    }

    #[test]
    fn ctrl_f_toggles_the_selected_app_server_pair() {
        let mut app = test_app();
        let bundle_path = temp_test_dir("favorite-bundle").join("config.yaml");
        app.config_bundle_path = Some(bundle_path.clone());
        let mut server = test_server("local", "localhost");
        server.app_servers = vec![AppServerEndpoint::app_services_default()];
        server.app_servers[0].favorite = true;
        app.config.servers = vec![server];
        app.open_servers_interface();

        assert_eq!(
            app.servers_interface_focus,
            ServersInterfaceFocus::Favorites
        );
        assert_eq!(app.servers_interface_favorites_state.selected(), Some(0));

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert!(app.servers_interface_favorites.is_empty());
        assert_eq!(app.servers_interface_favorites_state.selected(), None);
        assert!(!app.config.servers[0].app_servers[0].favorite);

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        )
        .unwrap();

        assert_eq!(app.servers_interface_favorites.len(), 1);
        assert!(app.config.servers[0].app_servers[0].favorite);

        let yaml = fs::read_to_string(&bundle_path).expect("read config bundle");
        let bundle: ConfigBundle = serde_yaml::from_str(&yaml).expect("parse config bundle");
        assert!(bundle.config.servers[0].app_servers[0].favorite);
    }

    #[test]
    fn app_server_row_opens_edit_form() {
        let mut app = test_app();
        let mut server = test_server("local", "localhost");
        server.app_servers = vec![AppServerEndpoint::app_services_default()];
        app.config.servers = vec![server];
        app.open_servers_interface();
        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::AppServerForm);
        assert_eq!(app.app_server_edit_target.as_deref(), Some("App-Services"));
    }

    #[test]
    fn servers_interface_renders_favorites_tree_and_context_hints() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = test_app();
        let mut server = test_server("local", "localhost");
        server.app_servers = vec![AppServerEndpoint::app_services_default()];
        server.app_servers[0].favorite = true;
        app.config.servers = vec![server];
        app.open_servers_interface();

        let mut terminal = Terminal::new(TestBackend::new(180, 40)).unwrap();
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(find_rendered_text(&buf, "Favorites (1)").is_some());
        assert!(find_rendered_text(&buf, "local → Default / App-Services").is_some());
        assert!(find_rendered_text(&buf, "Ctrl+F favorite").is_some());

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(find_rendered_text(&buf, "Enter fold/open").is_some());
        assert!(find_rendered_text(&buf, "Ctrl+F favorite").is_none());
        assert!(find_rendered_text(&buf, "Server Details").is_none());
        let name_x = find_rendered_text(&buf, "Name")
            .expect("name header is rendered")
            .0;
        let port_x = find_rendered_text(&buf, "Port")
            .expect("port header is rendered")
            .0;
        let secure_x = find_rendered_text(&buf, "Secure")
            .expect("secure header is rendered")
            .0;
        let active_x = find_rendered_text(&buf, "Active")
            .expect("active header is rendered")
            .0;
        let content_x = find_rendered_text(&buf, "Content DB")
            .expect("content DB header is rendered")
            .0;
        let modules_x = find_rendered_text(&buf, "Modules DB")
            .expect("modules DB header is rendered")
            .0;
        assert!(name_x < port_x);
        assert!(port_x < secure_x);
        assert!(secure_x < active_x);
        assert!(active_x < content_x);
        assert!(content_x < modules_x);

        let (x, y) = find_rendered_text(&buf, "▸ local").expect("selected server is rendered");
        let cell = buf.cell((x, y)).unwrap();
        assert_eq!(cell.fg, Color::Cyan);
        assert!(cell.modifier.contains(Modifier::BOLD));

        app.servers_interface_tree_state.select(None);
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let (x, y) = find_rendered_text(&buf, "▸ local").expect("server row is rendered");
        let cell = buf.cell((x, y)).unwrap();
        assert_eq!(cell.fg, Color::Cyan);
        assert!(cell.modifier.contains(Modifier::BOLD));
        app.servers_interface_tree_state.select(Some(0));

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(find_rendered_text(&buf, "▾ local").is_some());
        assert!(find_rendered_text(&buf, "+ Default").is_some());
        assert!(find_rendered_text(&buf, "- App-Services").is_some());
        assert!(find_rendered_text(&buf, "Ctrl+F favorite").is_some());
        assert!(find_rendered_text(&buf, "8000").is_some());
        assert!(find_rendered_text(&buf, "Documents").is_some());
        assert!(find_rendered_text(&buf, "Modules").is_some());
    }

    #[test]
    fn servers_tree_groups_app_servers_by_their_group() {
        let mut app = test_app();
        let mut server = test_server("grouped", "localhost");
        server.app_servers = vec![
            AppServerEndpoint {
                name: "App-Services".to_string(),
                port: 8000,
                secure: false,
                group: "Default".to_string(),
                content_database: Some("Documents".to_string()),
                modules_database: Some("Modules".to_string()),
                favorite: false,
            },
            AppServerEndpoint {
                name: "App-Services".to_string(),
                port: 8010,
                secure: true,
                group: "Group2".to_string(),
                content_database: Some("Group2-Content".to_string()),
                modules_database: Some("Group2-Modules".to_string()),
                favorite: false,
            },
        ];
        app.config.servers = vec![server];
        app.open_servers_interface();
        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(
            app.servers_interface_tree,
            vec![
                ServersInterfaceRow::Server {
                    server_name: "grouped".to_string(),
                },
                ServersInterfaceRow::Group {
                    server_name: "grouped".to_string(),
                    group_name: "Default".to_string(),
                },
                ServersInterfaceRow::AppServer {
                    server_name: "grouped".to_string(),
                    group_name: "Default".to_string(),
                    app_server_name: "App-Services".to_string(),
                },
                ServersInterfaceRow::Group {
                    server_name: "grouped".to_string(),
                    group_name: "Group2".to_string(),
                },
                ServersInterfaceRow::AppServer {
                    server_name: "grouped".to_string(),
                    group_name: "Group2".to_string(),
                    app_server_name: "App-Services".to_string(),
                },
            ]
        );
    }

    #[test]
    fn discovery_copy_adds_and_updates_marked_entries_without_removing_local_entries() {
        let mut app = test_app();
        let bundle_path = temp_test_dir("discovery-bundle").join("config.yaml");
        app.config_bundle_path = Some(bundle_path.clone());
        let mut server = test_server("local", "localhost");
        server.app_servers = vec![
            AppServerEndpoint {
                name: "keep".to_string(),
                port: 8000,
                secure: false,
                group: "Default".to_string(),
                content_database: Some("Old-Documents".to_string()),
                modules_database: Some("Old-Modules".to_string()),
                favorite: true,
            },
            AppServerEndpoint {
                name: "local-only".to_string(),
                port: 8123,
                secure: false,
                group: "Default".to_string(),
                content_database: None,
                modules_database: None,
                favorite: true,
            },
        ];
        app.config.servers = vec![server];
        app.app_server_discovery_parent = Some("local".to_string());
        app.app_server_discovery_results = vec![
            AppServerEndpoint {
                name: "keep".to_string(),
                port: 8010,
                secure: true,
                group: "Default".to_string(),
                content_database: Some("Documents".to_string()),
                modules_database: Some("Modules".to_string()),
                favorite: false,
            },
            AppServerEndpoint {
                name: "skip".to_string(),
                port: 8020,
                secure: true,
                group: "Group2".to_string(),
                content_database: Some("Other".to_string()),
                modules_database: Some("Other-Modules".to_string()),
                favorite: false,
            },
        ];
        app.app_server_discovery_state.select(Some(0));
        app.mode = AppMode::AppServerDiscovery;

        crate::events::handle_app_server_discovery_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .unwrap();
        crate::events::handle_app_server_discovery_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(app.config.servers[0].app_servers.len(), 2);
        let keep = app.config.servers[0]
            .app_servers
            .iter()
            .find(|endpoint| endpoint.name == "keep")
            .expect("marked endpoint should remain");
        assert_eq!(keep.port, 8010);
        assert!(keep.secure);
        assert_eq!(keep.content_database.as_deref(), Some("Documents"));
        assert_eq!(keep.modules_database.as_deref(), Some("Modules"));
        assert!(keep.favorite);
        let local_only = app.config.servers[0]
            .app_servers
            .iter()
            .find(|endpoint| endpoint.name == "local-only")
            .expect("unchecked local endpoint should be preserved");
        assert_eq!(local_only.port, 8123);
        assert!(local_only.favorite);
        assert!(app.expanded_servers.contains("local"));

        let yaml = fs::read_to_string(&bundle_path).expect("read config bundle");
        let bundle: ConfigBundle = serde_yaml::from_str(&yaml).expect("parse config bundle");
        assert_eq!(bundle.config.servers[0].app_servers.len(), 2);
        assert!(
            bundle.config.servers[0]
                .app_servers
                .iter()
                .any(|endpoint| endpoint.name == "keep" && endpoint.port == 8010)
        );
        assert!(
            bundle.config.servers[0]
                .app_servers
                .iter()
                .any(|endpoint| endpoint.name == "local-only" && endpoint.favorite)
        );
    }

    #[test]
    fn favorites_scroll_after_ten_entries_and_small_layouts_stay_valid() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = test_app();
        let mut server = test_server("local", "localhost");
        server.app_servers = (0..11)
            .map(|index| AppServerEndpoint {
                name: format!("fav-{index:02}"),
                port: 8000 + index as u16,
                secure: false,
                group: crate::client::default_app_server_group(),
                content_database: None,
                modules_database: None,
                favorite: true,
            })
            .collect();
        app.config.servers = vec![server];
        app.open_servers_interface();
        app.servers_interface_favorites_state.select(Some(10));

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(find_rendered_text(&buf, "fav-10").is_some());
        assert!(find_rendered_text(&buf, "fav-00").is_none());

        for (width, height) in [(80, 24), (40, 10), (20, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        }
    }

    #[test]
    fn app_server_activation_uses_the_selected_parent_server() {
        let mut app = test_app();
        let mut first = test_server("first", "one.example");
        first.app_servers = vec![AppServerEndpoint {
            name: "App-Services".to_string(),
            port: 8010,
            secure: false,
            group: crate::client::default_app_server_group(),
            content_database: Some("First-DB".to_string()),
            modules_database: Some("First-Modules".to_string()),
            favorite: false,
        }];
        let mut second = test_server("second", "two.example");
        second.app_servers = vec![AppServerEndpoint {
            name: "App-Services".to_string(),
            port: 8020,
            secure: true,
            group: crate::client::default_app_server_group(),
            content_database: Some("Second-DB".to_string()),
            modules_database: Some("Second-Modules".to_string()),
            favorite: false,
        }];
        app.config.servers = vec![first, second];
        app.open_servers_interface();

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.config.active_server.as_deref(), Some("second"));
        assert_eq!(
            app.config.active_app_server.as_deref(),
            Some("App-Services")
        );
        assert_eq!(app.config.active_database.as_deref(), Some("Second-DB"));
        assert_eq!(app.config.servers[0].port, 8000);
        assert_eq!(app.config.servers[1].port, 8020);
        assert!(app.config.servers[1].secure);
    }

    #[test]
    fn module_search_state_initializes_empty() {
        let app = test_app();
        assert!(app.module_search.pattern.is_empty());
        assert!(app.module_search.results.is_empty());
        assert!(app.module_search.total.is_none());
        assert!(app.module_search.focus_input);
        assert!(app.module_search.marked.is_empty());
    }

    fn module_search_app_with_results() -> App {
        let mut app = test_app();
        app.module_search.results = vec![
            "/a/get.xqy".to_string(),
            "/b/get.xqy".to_string(),
            "/c/other.xqy".to_string(),
        ];
        app.module_search.selected.select(Some(0));
        app
    }

    #[test]
    fn toggle_mark_highlighted_module_marks_and_unmarks() {
        let mut app = module_search_app_with_results();
        app.toggle_mark_highlighted_module();
        assert_eq!(app.module_search.marked, vec!["/a/get.xqy".to_string()]);
        app.toggle_mark_highlighted_module();
        assert!(app.module_search.marked.is_empty());
    }

    #[test]
    fn marked_modules_survive_new_search_results() {
        let mut app = module_search_app_with_results();
        app.toggle_mark_highlighted_module();
        // A fresh search replaces results and cursor but keeps marks.
        app.module_search.results = vec!["/d/new.xqy".to_string()];
        app.module_search.selected.select(Some(0));
        app.toggle_mark_highlighted_module();
        assert_eq!(
            app.module_search.marked,
            vec!["/a/get.xqy".to_string(), "/d/new.xqy".to_string()]
        );
    }

    #[test]
    fn mark_all_visible_modules_skips_duplicates() {
        let mut app = module_search_app_with_results();
        app.toggle_mark_highlighted_module();
        app.mark_all_visible_modules();
        assert_eq!(app.module_search.marked.len(), 3);
        app.mark_all_visible_modules();
        assert_eq!(app.module_search.marked.len(), 3);
        app.clear_marked_modules();
        assert!(app.module_search.marked.is_empty());
    }

    #[test]
    fn save_marked_modules_with_empty_marks_stays_open() {
        let mut app = module_search_app_with_results();
        app.mode = AppMode::ModuleCloneSelect;
        app.save_marked_modules();
        assert_eq!(app.mode, AppMode::ModuleCloneSelect);
        assert!(app.status_message.contains("Nothing marked"));
    }

    fn module_status_for(
        uri: &str,
        working_path: PathBuf,
    ) -> super::modules_cache::CachedModuleStatus {
        super::modules_cache::CachedModuleStatus {
            uri: uri.to_string(),
            rel: uri.trim_start_matches('/').to_string(),
            working_path,
            dirty: false,
            unsynced: false,
            missing_original: false,
        }
    }

    fn module_tree_test_app() -> (App, PathBuf) {
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        let cache_dir = temp_test_dir("module-tree");
        app.query_result_cache_dir = cache_dir.clone();
        let root = super::modules_cache::modules_db_root(&cache_dir, "M");
        app.cached_modules = vec![
            module_status_for("/endpoints/a/get.xqy", root.join("endpoints/a/get.xqy")),
            module_status_for("/endpoints/b/get.xqy", root.join("endpoints/b/get.xqy")),
        ];
        (app, cache_dir)
    }

    #[test]
    fn module_tree_rows_group_by_hierarchy() {
        let (app, _dir) = module_tree_test_app();
        let rows = app.module_tree_rows();
        let labels: Vec<(String, bool, usize)> = rows
            .iter()
            .map(|row| {
                (
                    row.path.file_name().unwrap().to_string_lossy().to_string(),
                    row.is_dir,
                    row.depth,
                )
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                ("endpoints".to_string(), true, 0),
                ("a".to_string(), true, 1),
                ("get.xqy".to_string(), false, 2),
                ("b".to_string(), true, 1),
                ("get.xqy".to_string(), false, 2),
            ]
        );
    }

    #[test]
    fn toggle_module_folder_hides_children() {
        let (mut app, _dir) = module_tree_test_app();
        let folder = app.module_tree_rows()[0].path.clone();
        assert!(app.module_tree_rows()[0].is_dir);
        app.toggle_module_folder(&folder);
        assert_eq!(app.module_tree_rows().len(), 1);
        app.toggle_module_folder(&folder);
        assert_eq!(app.module_tree_rows().len(), 5);
    }

    #[test]
    fn selected_cached_module_returns_none_for_folder_row() {
        let (mut app, _dir) = module_tree_test_app();
        app.cached_modules_state.select(Some(0));
        assert!(app.selected_cached_module().is_none());
        app.cached_modules_state.select(Some(2));
        assert_eq!(
            app.selected_cached_module().map(|s| s.uri),
            Some("/endpoints/a/get.xqy".to_string())
        );
    }

    #[test]
    fn open_module_row_loads_file_on_visit() {
        let (mut app, dir) = module_tree_test_app();
        let working = app.cached_modules[1].working_path.clone();
        fs::create_dir_all(working.parent().unwrap()).unwrap();
        fs::write(&working, "'use strict';\n1").unwrap();
        // Row 4 is the second file (endpoints/b/get.xqy at depth 2).
        app.open_module_row(4);
        assert_eq!(app.active_query_file, Some(working));
        assert!(app.query_text().contains('1'));
        // Visiting a folder row moves without loading.
        app.open_module_row(0);
        assert_eq!(app.cached_modules_state.selected(), Some(0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn toggle_to_files_syncs_selection_to_active_file() {
        let (mut app, root) = query_tree_app("toggle-files");
        write_query(&root.join("a.xqy"));
        write_query(&root.join("sub/b.xqy"));
        app.refresh_query_files().unwrap();
        // Stale selection on the first row while the editor holds b.xqy.
        app.active_query_file = Some(root.join("sub/b.xqy"));
        app.query_file_list_state.select(Some(0));
        app.modules_view = true;
        app.toggle_modules_view();
        assert!(!app.modules_view);
        let row = app.selected_query_row().expect("row selected");
        assert_eq!(row.path, root.join("sub/b.xqy"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn toggle_to_modules_selects_active_cached_module() {
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        let dir = temp_test_dir("toggle-modules");
        app.query_result_cache_dir = dir.clone();
        super::modules_cache::checkout_module(&dir, "M", "/endpoints/a/get.xqy", "v1").unwrap();
        super::modules_cache::checkout_module(&dir, "M", "/endpoints/b/get.xqy", "v1").unwrap();
        // Collapse the parent so the reveal path is exercised.
        app.refresh_cached_modules();
        let folder = app.module_tree_rows()[0].path.clone();
        app.toggle_module_folder(&folder);
        assert_eq!(app.module_tree_rows().len(), 1);
        // Editor holds the second module while the selection is stale.
        let working = super::modules_cache::working_path_for(&dir, "M", "/endpoints/b/get.xqy");
        app.active_query_file = Some(working.clone());
        app.cached_modules_state.select(Some(0));
        app.modules_view = false;
        app.toggle_modules_view();
        assert!(app.modules_view);
        let row = app.selected_module_row().expect("row selected");
        assert!(!row.is_dir);
        assert_eq!(row.path, working);
        // Collapsed ancestors were expanded by the reveal.
        assert!(app.collapsed_module_folders.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn modules_view_blocks_file_actions_with_message() {
        let (mut app, _dir) = module_tree_test_app();
        app.modules_view = true;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert!(app.handle_modules_sidebox_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)));
        assert!(
            app.status_message
                .contains("Not available for cached modules")
        );
        assert!(
            app.handle_modules_sidebox_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT))
        );
        assert!(
            app.status_message
                .contains("Not available for cached modules")
        );
    }

    #[test]
    fn create_module_from_invalid_uri_stays_open() {
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        let dir = temp_test_dir("module-create-invalid");
        app.query_result_cache_dir = dir.clone();
        app.mode = AppMode::ModuleCreate;
        app.new_module_uri_input = "relative/path.xqy".to_string();
        app.create_module_from_input();
        assert_eq!(app.mode, AppMode::ModuleCreate);
        assert!(app.status_message.contains("Invalid module URI"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn create_module_stages_loads_and_selects() {
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        let dir = temp_test_dir("module-create");
        app.query_result_cache_dir = dir.clone();
        app.mode = AppMode::ModuleCreate;
        app.new_module_uri_input = "/endpoints/new/get.xqy".to_string();
        app.create_module_from_input();
        assert_eq!(app.mode, AppMode::Normal);
        let working = super::modules_cache::working_path_for(&dir, "M", "/endpoints/new/get.xqy");
        assert!(working.is_file());
        assert_eq!(app.active_query_file, Some(working.clone()));
        let row = app.selected_module_row().expect("row selected");
        assert_eq!(row.path, working);
        assert!(
            app.status_message
                .contains("Created /endpoints/new/get.xqy")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reset_uses_fresh_dirty_state_before_client_check() {
        // On-disk edit with stale in-memory statuses (clean): reset must
        // recompute before deciding, even when it then bails on client.
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        let dir = temp_test_dir("reset-fresh");
        app.query_result_cache_dir = dir.clone();
        super::modules_cache::checkout_module(&dir, "M", "/r/get.xqy", "v1").unwrap();
        app.refresh_cached_modules();
        assert!(!app.cached_modules[0].dirty);
        let working = app.cached_modules[0].working_path.clone();
        fs::write(&working, "edited").unwrap();
        // Stale: in-memory statuses still claim clean.
        assert!(!app.cached_modules[0].dirty);
        app.cached_modules_state.select(Some(1));
        app.client = None;
        app.reset_selected_to_original();
        assert!(app.cached_modules[0].dirty);
        assert_eq!(app.status_message, "No server connected.");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn module_working_copies_are_isolated_by_environment() {
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        let dir = temp_test_dir("module-environments");
        app.query_result_cache_dir = dir.clone();

        app.client = Some(MarkLogicClient::new(test_server("dev", "dev.example")).unwrap());
        let dev_cache = app.modules_cache_dir();
        super::modules_cache::checkout_module(&dev_cache, "M", "/x/get.xqy", "dev").unwrap();
        app.refresh_cached_modules();
        assert_eq!(app.cached_modules.len(), 1);

        app.client = Some(MarkLogicClient::new(test_server("test", "test.example")).unwrap());
        let test_cache = app.modules_cache_dir();
        assert_ne!(dev_cache, test_cache);
        app.refresh_cached_modules();
        assert!(app.cached_modules.is_empty());
        super::modules_cache::checkout_module(&test_cache, "M", "/x/get.xqy", "test").unwrap();

        app.client = Some(MarkLogicClient::new(test_server("dev", "dev.example")).unwrap());
        app.refresh_cached_modules();
        assert_eq!(app.cached_modules.len(), 1);
        assert_eq!(
            fs::read_to_string(&app.cached_modules[0].working_path).unwrap(),
            "dev"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reconnect_refreshes_modules_for_the_active_environment() {
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        app.config.servers = vec![
            test_server("dev", "dev.example"),
            test_server("test", "test.example"),
        ];
        app.config.active_server = Some("dev".to_string());
        let dir = temp_test_dir("module-environment-switch");
        app.query_result_cache_dir = dir.clone();

        app.reconnect();
        let dev_cache = app.modules_cache_dir();
        super::modules_cache::checkout_module(&dev_cache, "M", "/x/get.xqy", "dev").unwrap();
        app.refresh_cached_modules();
        assert_eq!(app.cached_modules.len(), 1);

        app.config.active_server = Some("test".to_string());
        app.reconnect();

        assert!(app.cached_modules.is_empty());
        assert_eq!(app.cached_modules_state.selected(), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn confirm_drop_cached_selected_removes_and_clears_editor() {
        let mut app = test_app();
        app.config.active_modules_database = Some("M".to_string());
        let dir = temp_test_dir("module-drop");
        app.query_result_cache_dir = dir.clone();
        app.query_root_dir = dir.clone();
        super::modules_cache::checkout_module(&dir, "M", "/d/f.xqy", "v1").unwrap();
        app.refresh_cached_modules();
        // Editor shows the file about to be dropped.
        let working = super::modules_cache::working_path_for(&dir, "M", "/d/f.xqy");
        app.active_query_file = Some(working.clone());
        app.module_drop_target = Some(("/d/f.xqy".to_string(), false, false));
        app.mode = AppMode::ModuleDropCacheConfirm;
        app.confirm_drop_cached_selected();
        assert!(!working.exists());
        assert!(app.active_query_file.is_none());
        assert!(app.cached_modules.is_empty());
        assert!(app.status_message.contains("Dropped local cached copy"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn top_bar_flows_hints_across_full_width() {
        use ratatui::{Terminal, backend::TestBackend};

        fn rendered_top_row(width: u16) -> String {
            let mut app = test_app();
            app.mode = AppMode::Normal;
            app.query_visible = true;
            app.focus_query_panel();
            let backend = TestBackend::new(width, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::ui(f, &mut app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .take(width as usize)
                .map(|cell| cell.symbol())
                .collect()
        }

        // At 100 columns the old fixed 50/50 right-aligned bar clipped the
        // start of the hints; now the leading labels must be visible.
        let narrow = rendered_top_row(100);
        for label in ["Insert", "Files↔Mods", "Modules", "Editor"] {
            assert!(narrow.contains(label), "missing {label} in {narrow}");
        }
        // Wide screen: the whole hint set fits, nothing hidden.
        let wide = rendered_top_row(220);
        for label in [
            "Insert",
            "Files↔Mods",
            "Modules",
            "Editor",
            "Multi-run",
            "Cancel",
            "Folders",
            "Rename",
        ] {
            assert!(wide.contains(label), "missing {label} in {wide}");
        }
    }

    #[test]
    fn terminal_focus_events_dim_and_restore_chrome() {
        use crossterm::event::Event;
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer().cell((0, 0)).unwrap().bg,
            Color::Rgb(160, 150, 110)
        );
        assert_eq!(
            terminal.backend().buffer().cell((0, 1)).unwrap().fg,
            Color::Yellow
        );

        crate::events::handle_terminal_event(&mut app, Event::FocusLost).unwrap();
        assert!(!app.terminal_focused);
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer().cell((0, 0)).unwrap().bg,
            Color::DarkGray
        );
        assert_eq!(
            terminal.backend().buffer().cell((0, 1)).unwrap().fg,
            Color::DarkGray
        );

        crate::events::handle_terminal_event(&mut app, Event::FocusGained).unwrap();
        assert!(app.terminal_focused);
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer().cell((0, 0)).unwrap().bg,
            Color::Rgb(160, 150, 110)
        );
        assert_eq!(
            terminal.backend().buffer().cell((0, 1)).unwrap().fg,
            Color::Yellow
        );
    }

    #[test]
    fn modules_and_files_views_share_one_sidebar_width() {
        let (mut tree_app, _dir) = module_tree_test_app();
        let width = ui::sidebar_width(&tree_app);
        tree_app.modules_view = true;
        assert_eq!(ui::sidebar_width(&tree_app), width);
        tree_app.modules_view = false;
        assert_eq!(ui::sidebar_width(&tree_app), width);
    }

    #[test]
    fn sidebar_width_has_no_modules_minimum() {
        let mut app = test_app();
        app.modules_view = true;
        app.cached_modules = Vec::new();
        assert_eq!(ui::sidebar_width(&app), 15);
    }

    #[test]
    fn format_sync_comment_xquery() {
        assert_eq!(
            format_sync_comment("Foo-modules", "/path/to/uri.xml", true),
            "(:!%sync%|DB[Foo-modules]|URI[/path/to/uri.xml] !:)"
        );
    }

    #[test]
    fn format_sync_comment_javascript() {
        assert_eq!(
            format_sync_comment("Foo-modules", "/path/to/uri.sjs", false),
            "/* !%sync%|DB[Foo-modules]|URI[/path/to/uri.sjs] ! */"
        );
    }

    #[test]
    fn parse_sync_comment_reads_xquery_marker() {
        let content = "(:!%sync%|DB[Foo-modules]|URI[/path/to/uri.xml] !:)\nhello";
        assert_eq!(
            parse_sync_comment(content),
            Some(SyncInfo {
                db: "Foo-modules".to_string(),
                uri: "/path/to/uri.xml".to_string(),
            })
        );
    }

    #[test]
    fn parse_sync_comment_reads_javascript_marker() {
        let content = "/* !%sync%|DB[Foo-modules]|URI[/path/to/uri.sjs] ! */\n'use strict';";
        assert_eq!(
            parse_sync_comment(content),
            Some(SyncInfo {
                db: "Foo-modules".to_string(),
                uri: "/path/to/uri.sjs".to_string(),
            })
        );
    }

    #[test]
    fn parse_sync_comment_returns_none_for_plain_file() {
        let content = "xquery version \"1.0-ml\";\n1";
        assert_eq!(parse_sync_comment(content), None);
    }

    #[test]
    fn strip_sync_comment_removes_first_marker() {
        let content = "(:!%sync%|DB[Foo-modules]|URI[/path/to/uri.xml] !:)\nhello\nworld";
        assert_eq!(strip_sync_comment(content), "hello\nworld");
    }

    #[test]
    fn strip_sync_comment_leaves_other_lines() {
        let content = "(:!%sync%|DB[Foo-modules]|URI[/a] !:)\n(: a normal comment :)\n1";
        assert_eq!(strip_sync_comment(content), "(: a normal comment :)\n1");
    }

    #[test]
    fn is_xquery_file_detects_xqy_extension() {
        assert!(is_xquery_file(Path::new("foo.xqy")));
        assert!(!is_xquery_file(Path::new("foo.sjs")));
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

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        )
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
    fn results_format_picker_defers_uri_lookup_until_selection_is_confirmed() {
        let mut app = test_app();
        app.set_query_results(vec!["/documents/example.json".to_string()]);

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT),
        );
        assert_eq!(app.mode, AppMode::QueryResultFormatSelect);
        assert_eq!(app.query_results_format, QueryResultsFormat::Plain);
        assert!(app.uri_query_results.is_empty());

        app.query_results_format_picker_state
            .select(Some(QueryResultsFormat::Uris.picker_index()));
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.query_results_format, QueryResultsFormat::Uris);
        assert_eq!(app.uri_query_results.len(), 1);
        assert_eq!(app.uri_query_results[0].uri, "/documents/example.json");
        assert!(app.uri_query_results[0].error.is_some());

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.mode, AppMode::DeleteConfirm);
        assert_eq!(app.delete_uris(), vec!["/documents/example.json"]);
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.query_uri_delete_target.is_none());

        app.open_query_results_format_picker();
        app.query_results_format_picker_state
            .select(Some(QueryResultsFormat::Json.picker_index()));
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.query_results_format, QueryResultsFormat::Json);
    }

    #[test]
    fn status_line_starts_with_highlighted_environment() {
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

        let spans = app.status_line().spans;

        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("/test/retention/set/r"),
            "collection should not appear in status"
        );
        assert!(text.starts_with("dockerlocal:8000 ▾"));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
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
        assert!(
            text.contains("dockerlocal:8000"),
            "server and port should appear"
        );
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

        assert!(text.starts_with("srv:8010 ▾"));
        assert!(text.contains("NORMAL"));
        assert!(text.ends_with("db"));
        assert!(text.contains("srv:8010"), "expected srv:8010 in: {}", text);
        let parts: Vec<&str> = text.split(" · ").collect();
        assert_eq!(
            parts.len(),
            2,
            "should contain the environment and mode, followed by the database"
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

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::HelpOverlay);
        assert_eq!(app.previous_mode, Some(AppMode::FullScreenView));
    }

    #[test]
    fn question_opens_help_from_modal_mode_and_restores_it() {
        let mut app = test_app();
        app.mode = AppMode::QueryFileSelect;

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::HelpOverlay);
        assert_eq!(app.previous_mode, Some(AppMode::QueryFileSelect));

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::QueryFileSelect);
        assert_eq!(app.previous_mode, None);
    }

    #[test]
    fn question_types_into_text_input_modes() {
        let mut app = test_app();
        app.mode = AppMode::ServerForm;

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::ServerForm);
        assert_eq!(app.server_form_fields[0], "?");
    }

    #[test]
    fn question_in_module_clone_depends_on_search_focus() {
        let mut app = test_app();
        app.mode = AppMode::ModuleCloneSelect;
        app.module_search.focus_input = true;

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::ModuleCloneSelect);
        assert_eq!(app.module_search.pattern, "?");

        app.module_search.focus_input = false;
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::HelpOverlay);
        assert_eq!(app.previous_mode, Some(AppMode::ModuleCloneSelect));
    }

    #[test]
    fn question_in_confirm_dialog_opens_help() {
        let mut app = test_app();
        app.mode = AppMode::DeleteConfirm;

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::HelpOverlay);
        assert_eq!(app.previous_mode, Some(AppMode::DeleteConfirm));
    }

    #[test]
    fn fullscreen_q_returns_to_normal_mode() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.full_view_content = "test content".to_string();

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::Normal);
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
    fn query_panel_p_previews_compiled_query_in_navigate_mode() {
        let mut app = test_app();
        app.focus = Focus::Query;
        app.edit_mode = EditMode::Navigate;
        app.query_editor = App::new_query_editor(vec!["1 + 1".to_string()]);

        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );

        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        assert!(
            app.needs_terminal_refresh,
            "p in navigate mode should open compiled query preview in external editor"
        );
    }

    #[test]
    fn query_panel_p_does_not_preview_in_insert_mode() {
        let mut app = test_app();
        app.focus = Focus::Query;
        app.edit_mode = EditMode::Insert;
        app.query_editor = App::new_query_editor(vec!["test content".to_string()]);

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );

        assert!(
            !app.needs_terminal_refresh,
            "p in insert mode should type letter, not open preview"
        );
        let text: String = app.query_text();
        assert!(text.contains('p'), "p should be inserted in insert mode");
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

        assert_eq!(app.mode, AppMode::MarkLogicLogs);
    }

    #[test]
    fn app_logs_command_keeps_diagnostic_viewer_available() {
        let mut app = test_app();
        app.command_input = ":app-logs".into();
        app.execute_command();
        assert_eq!(app.mode, AppMode::System);
    }

    #[test]
    fn server_log_search_accepts_navigation_and_help_characters_as_text() {
        let mut app = test_app();
        app.command_input = ":logs".into();
        app.execute_command();
        app.server_logs.files_focus = false;
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        )
        .unwrap();
        for c in ['q', '?', ':'] {
            handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        assert_eq!(app.server_logs.search, "q?:");
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn server_log_search_escape_clears_an_in_progress_filter() {
        let mut app = test_app();
        app.command_input = ":logs".into();
        app.execute_command();
        app.server_logs.files_focus = false;
        app.server_logs.lines = vec!["first log entry".into(), "second log entry".into()];
        app.server_logs.filter();

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(app.server_logs.matches.is_empty());

        handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();

        assert!(!app.server_logs.search_editing);
        assert!(app.server_logs.search.is_empty());
        assert_eq!(app.server_logs.matches, vec![0, 1]);
    }

    #[test]
    fn server_log_content_uses_page_down_and_u_as_page_keys() {
        let mut app = test_app();
        app.command_input = ":logs".into();
        app.execute_command();
        app.server_logs.files_focus = false;
        app.server_logs.matches = (0..30).collect();
        app.server_logs.view_height = 7;

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_logs.scroll, 7);

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_logs.scroll, 0);
    }

    #[test]
    fn file_filter_and_port_picker_have_explicit_controls() {
        let mut app = test_app();
        app.mode = AppMode::MarkLogicLogs;
        app.server_logs.files_focus = true;
        app.server_logs.files = [("ErrorLog.txt", 0), ("8000_ErrorLog.txt", 10)]
            .into_iter()
            .map(|(name, size)| crate::logs::LogFile {
                host: "one".into(),
                path: format!("/Logs/{name}"),
                name: name.into(),
                size,
                modified: "2026-10-14T12:00:00Z".into(),
                present: true,
            })
            .collect();
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(app.server_logs.options_open && app.server_logs.options_ports);
        assert_eq!(app.server_logs.visible_file_indices().len(), 2);
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.server_logs.visible_file_indices().len(), 1);
        assert_eq!(app.server_logs.available_file_count(), 2);
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
        )
        .unwrap();
        assert_eq!(app.server_logs.visible_file_indices().len(), 2);
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        )
        .unwrap();
        for c in ['8', '0', '0', '0'] {
            handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(app.server_logs.visible_file_indices().len(), 1);
        assert_eq!(app.server_logs.available_file_count(), 2);
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_logs.file_filter, "8000?");
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.server_logs.visible_file_indices().len(), 2);
    }

    #[test]
    fn combined_logs_keep_controls_available_with_the_sidebar_hidden() {
        let mut app = test_app();
        app.mode = AppMode::MarkLogicLogs;
        for ch in ['m', '\\', 'p'] {
            handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert!(app.server_logs.combined);
        assert!(!app.server_logs.sidebar_visible);
        assert!(!app.server_logs.files_focus);
        assert!(app.server_logs.options_open && app.server_logs.options_ports);
        // Empty inventories also allow navigation and closing the selector.
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)).unwrap();
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        assert!(!app.server_logs.options_open);
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        )
        .unwrap();
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        assert_eq!(app.server_logs.time_input, "?");
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        assert!(!app.server_logs.time_editing);
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(app.server_logs.options_open && !app.server_logs.options_ports);
    }

    #[test]
    fn discovered_log_days_follow_file_list_keys() {
        let mut app = test_app();
        app.mode = AppMode::MarkLogicLogs;
        app.server_logs.show_untracked = true;
        app.server_logs.files_focus = true;
        for (key, expected) in [
            (KeyCode::Char('2'), 2),
            (KeyCode::Left, 3),
            (KeyCode::Right, 2),
            (KeyCode::Char('0'), 0),
            (KeyCode::Right, 0),
        ] {
            handle_key_event(&mut app, KeyEvent::new(key, KeyModifiers::NONE)).unwrap();
            assert_eq!(app.server_logs.discovery_days_ago, expected);
        }
        app.server_logs.files_focus = false;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.server_logs.horizontal, 8);
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

    fn mark_path_running(app: &mut App, path: &Path) -> tokio::task::JoinHandle<()> {
        let handle = app.rt.spawn(async {
            std::future::pending::<()>().await;
        });
        app.running_queries.insert(
            path.to_path_buf(),
            RunningQuery {
                started_at: Instant::now(),
                abort: handle.abort_handle(),
            },
        );
        handle
    }

    #[test]
    fn execute_query_on_running_file_shows_busy_without_spawning() {
        let mut app = test_app();
        let path = PathBuf::from("query-1.xqy");
        app.active_query_file = Some(path.clone());
        app.query_results = vec!["stale".to_string()];
        let handle = mark_path_running(&mut app, &path);

        app.execute_query();

        assert_eq!(app.results_text, "Busy, check back later");
        assert!(app.query_results.is_empty());
        assert_eq!(app.running_queries.len(), 1);

        handle.abort();
        let _ = app.rt.block_on(handle);
    }

    #[test]
    fn cancel_running_query_clears_entry_and_reports() {
        let mut app = test_app();
        let path = PathBuf::from("query-1.xqy");
        app.active_query_file = Some(path.clone());
        let handle = mark_path_running(&mut app, &path);

        app.cancel_running_query();

        assert!(app.running_queries.is_empty());
        assert_eq!(app.results_text, "Query cancelled.");
        assert!(app.status_message.contains("query-1.xqy"));

        handle.abort();
        let _ = app.rt.block_on(handle);
    }

    #[test]
    fn cancel_without_running_query_reports_status() {
        let mut app = test_app();
        app.active_query_file = Some(PathBuf::from("query-1.xqy"));

        app.cancel_running_query();

        assert_eq!(app.status_message, "No running query for this file.");
    }

    #[test]
    fn apply_completion_for_active_file_sets_results() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("completion-active");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.active_query_file = Some(path.clone());
        let handle = mark_path_running(&mut app, &path);

        app.apply_query_completion(QueryCompletion {
            path: path.clone(),
            started_at: Instant::now(),
            outcome: Ok(vec!["result-a".to_string()]),
            total_runs: 1,
        });

        assert!(app.running_queries.is_empty());
        assert_eq!(app.query_results, vec!["result-a".to_string()]);
        assert!(app.query_results_timestamp.is_some());
        assert!(
            temp_dir
                .join(".marklogic-tui")
                .join("query-1.xqy")
                .join("results-v1.json")
                .exists()
        );

        handle.abort();
        let _ = app.rt.block_on(handle);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn apply_completion_for_inactive_file_caches_snapshot() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("completion-inactive");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.active_query_file = Some(temp_dir.join("other.xqy"));
        let handle = mark_path_running(&mut app, &path);

        app.apply_query_completion(QueryCompletion {
            path: path.clone(),
            started_at: Instant::now(),
            outcome: Ok(vec!["result-a".to_string(), "result-b".to_string()]),
            total_runs: 1,
        });

        assert!(app.running_queries.is_empty());
        assert!(app.query_results.is_empty());
        assert!(
            app.status_message
                .contains("Query finished: query-1.xqy (2 results")
        );
        // Switching back to the file restores the cached background results.
        app.load_query_file(path).unwrap();
        assert_eq!(
            app.query_results,
            vec!["result-a".to_string(), "result-b".to_string()]
        );

        handle.abort();
        let _ = app.rt.block_on(handle);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn load_query_file_shows_busy_for_running_path() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("load-busy");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        let handle = mark_path_running(&mut app, &path);

        app.load_query_file(path).unwrap();

        assert_eq!(app.results_text, "Busy, check back later");
        assert!(app.query_results.is_empty());

        handle.abort();
        let _ = app.rt.block_on(handle);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn poll_query_completions_applies_queued_outcome() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("poll-completions");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.active_query_file = Some(path.clone());
        let handle = mark_path_running(&mut app, &path);

        app.query_tx
            .send(QueryEvent::Completion(QueryCompletion {
                path: path.clone(),
                started_at: Instant::now(),
                outcome: Ok(vec!["polled".to_string()]),
                total_runs: 1,
            }))
            .unwrap();
        app.poll_query_completions();

        assert!(app.running_queries.is_empty());
        assert_eq!(app.query_results, vec!["polled".to_string()]);

        handle.abort();
        let _ = app.rt.block_on(handle);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn format_query_error_includes_causes() {
        let err = anyhow::anyhow!("outer").context("middle");
        let text = format_query_error(err);
        assert!(text.starts_with("Query error: middle"));
        assert!(text.contains("Caused by: outer"));
    }

    #[test]
    fn active_variables_replace_known_tokens_and_leave_unknown_tokens() {
        let variables = BTreeMap::from([
            ("LIMIT".to_string(), "10".to_string()),
            ("URI".to_string(), "/example".to_string()),
        ]);
        assert_eq!(
            substitute_active_variables("%%URI%%?limit=%%LIMIT%%&missing=%%NOPE%%", &variables),
            "/example?limit=10&missing=%%NOPE%%"
        );
    }

    #[test]
    fn declared_variable_overrides_only_change_the_execution_copy() {
        let query = concat!(
            "xquery version \"1.0-ml\";\n",
            "declare variable $limit as xs:int := 10;\n",
            "declare variable $uri as xs:string := \"/default\";\n",
            "$limit"
        );
        let variables = declared_variables(query);
        assert_eq!(
            variables
                .iter()
                .map(|variable| (variable.name.as_str(), variable.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("limit", "10"), ("uri", "\"/default\"")]
        );

        let overrides = BTreeMap::from([("limit".to_string(), "25".to_string())]);
        let rewritten = apply_declared_variable_overrides(query, &overrides);
        assert!(rewritten.contains("$limit as xs:int :=25;"));
        assert!(rewritten.contains("$uri as xs:string := \"/default\";"));
    }

    #[test]
    fn ctrl_r_opens_declared_variable_overrides() {
        let mut app = test_app();
        app.replace_query_contents("declare variable $limit as xs:int := 10;\n$limit");
        app.focus_query_panel();

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );

        assert_eq!(app.mode, AppMode::DeclaredVariables);
        assert_eq!(
            app.declared_variable_fields,
            vec![("limit".to_string(), "10".to_string())]
        );
    }

    #[test]
    fn active_variable_editor_adds_and_saves_a_value() {
        let mut app = test_app();
        app.focus_query_panel();
        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.mode, AppMode::ActiveVariables);
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert_eq!(app.active_variable_fields.len(), 2);

        for ch in "LIMIT".chars() {
            handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::SHIFT),
            )
            .unwrap();
        }
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)).unwrap();
        for ch in "25".chars() {
            handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            )
            .unwrap();
        }
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();

        assert_eq!(
            app.config.active_variables.get("LIMIT"),
            Some(&"25".to_string())
        );
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn query_result_filter_uses_wildcards_and_formats_only_visible_results() {
        let mut app = test_app();
        app.set_query_results(vec![
            "/items/alpha.json".to_string(),
            "/items/alphabet.json".to_string(),
            "/items/beta.json".to_string(),
        ]);
        app.focus_results_panel();

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.focus, Focus::Filter);
        for ch in "*alpha*".chars() {
            handle_filter_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            );
        }
        handle_filter_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.visible_query_result_count(), 2);
        assert_eq!(app.query_results.len(), 3);

        app.open_query_results_format_picker();
        app.query_results_format_picker_state
            .select(Some(QueryResultsFormat::Uris.picker_index()));
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert_eq!(app.uri_query_results.len(), 2);
        assert!(
            app.uri_query_results
                .iter()
                .all(|result| result.uri.contains("alpha"))
        );
    }

    #[test]
    fn wildcard_matching_supports_question_marks_and_plain_text() {
        assert!(wildcard_matches("report-?.json", "report-a.json"));
        assert!(!wildcard_matches("report-?.json", "report-aa.json"));
        assert!(wildcard_matches("ALPHA", "/items/alpha.json"));
    }

    #[test]
    fn assemble_multi_run_results_orders_newest_first() {
        let blocks = vec![
            (1, vec!["r1a".to_string(), "r1b".to_string()], None),
            (2, vec!["r2a".to_string()], None),
            (3, vec!["r3a".to_string()], None),
        ];

        assert_eq!(
            assemble_multi_run_results(&blocks),
            vec![
                "r3a",
                "--- MULTI-RUN #2 ---",
                "r2a",
                "--- MULTI-RUN #1 ---",
                "r1a",
                "r1b",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn assemble_multi_run_results_single_run_has_no_separator() {
        let blocks = vec![(1, vec!["only".to_string()], None)];

        assert_eq!(
            assemble_multi_run_results(&blocks),
            vec!["only".to_string()]
        );
    }

    #[test]
    fn assemble_multi_run_results_failed_run_keeps_partial() {
        let blocks = vec![
            (1, vec!["r1".to_string()], None),
            (2, Vec::new(), Some("Query error: boom".to_string())),
        ];

        assert_eq!(
            assemble_multi_run_results(&blocks),
            vec![
                "--- MULTI-RUN #2 FAILED ---",
                "Query error: boom",
                "--- MULTI-RUN #1 ---",
                "r1",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn open_multi_run_on_running_file_shows_busy() {
        let mut app = test_app();
        let path = PathBuf::from("query-1.xqy");
        app.active_query_file = Some(path.clone());
        let handle = mark_path_running(&mut app, &path);

        app.open_multi_run();

        assert_eq!(app.results_text, "Busy, check back later");
        assert_eq!(app.mode, AppMode::Normal);

        handle.abort();
        let _ = app.rt.block_on(handle);
    }

    #[test]
    fn open_multi_run_prefills_last_values() {
        let mut app = test_app();
        app.active_query_file = Some(PathBuf::from("query-1.xqy"));
        app.last_multi_run_count = 7;
        app.last_multi_run_wait_secs = 1.5;

        app.open_multi_run();

        assert_eq!(app.mode, AppMode::MultiRunForm);
        assert_eq!(
            app.multi_run_form_fields,
            vec!["7".to_string(), "1.5".to_string()]
        );
        assert_eq!(app.multi_run_form_step, 0);
    }

    #[test]
    fn submit_multi_run_form_rejects_invalid_input() {
        let mut app = test_app();
        app.mode = AppMode::MultiRunForm;

        for (count, wait) in [
            ("0", "0"),
            ("abc", "0"),
            ("1001", "0"),
            ("3", "-1"),
            ("3", "x"),
        ] {
            app.multi_run_form_fields = vec![count.to_string(), wait.to_string()];
            app.submit_multi_run_form();
            assert_eq!(
                app.mode,
                AppMode::MultiRunForm,
                "form should stay open for ({}, {})",
                count,
                wait
            );
            assert!(
                !app.status_message.is_empty(),
                "status should explain the rejection"
            );
        }
        assert!(app.running_queries.is_empty());
    }

    #[test]
    fn apply_query_progress_updates_status_while_tracked() {
        let mut app = test_app();
        let path = PathBuf::from("query-1.xqy");

        // Stale progress for an untracked path is ignored.
        app.apply_query_progress(QueryProgress {
            path: path.clone(),
            run: 2,
            total: 5,
            cumulative: vec!["stale".to_string()],
        });
        assert!(app.status_message.is_empty());
        assert!(app.query_results.is_empty());

        let handle = mark_path_running(&mut app, &path);
        app.apply_query_progress(QueryProgress {
            path: path.clone(),
            run: 2,
            total: 5,
            cumulative: vec!["stale".to_string()],
        });
        assert!(app.status_message.contains("Multi-run 2/5"));

        handle.abort();
        let _ = app.rt.block_on(handle);
    }

    #[test]
    fn apply_query_progress_streams_cumulative_results_to_active_file() {
        let mut app = test_app();
        let path = PathBuf::from("query-1.xqy");
        app.active_query_file = Some(path.clone());
        let handle = mark_path_running(&mut app, &path);

        app.apply_query_progress(QueryProgress {
            path: path.clone(),
            run: 1,
            total: 3,
            cumulative: vec!["r1".to_string()],
        });
        assert_eq!(app.query_results, vec!["r1".to_string()]);
        assert!(app.query_results_timestamp.is_none());

        app.apply_query_progress(QueryProgress {
            path: path.clone(),
            run: 2,
            total: 3,
            cumulative: vec![
                "r2".to_string(),
                "--- MULTI-RUN #1 ---".to_string(),
                "r1".to_string(),
            ],
        });
        assert_eq!(
            app.query_results,
            vec![
                "r2".to_string(),
                "--- MULTI-RUN #1 ---".to_string(),
                "r1".to_string(),
            ]
        );
        assert!(app.query_results_timestamp.is_none());

        handle.abort();
        let _ = app.rt.block_on(handle);
    }

    #[test]
    fn apply_query_progress_for_inactive_file_only_caches_partial() {
        let mut app = test_app();
        let path = PathBuf::from("query-1.xqy");
        app.active_query_file = Some(PathBuf::from("other.xqy"));
        let handle = mark_path_running(&mut app, &path);

        app.apply_query_progress(QueryProgress {
            path: path.clone(),
            run: 1,
            total: 2,
            cumulative: vec!["r1".to_string()],
        });

        assert!(app.query_results.is_empty());
        assert_eq!(
            app.running_partial_results.get(&path),
            Some(&vec!["r1".to_string()])
        );

        handle.abort();
        let _ = app.rt.block_on(handle);
    }

    #[test]
    fn load_query_file_shows_partial_results_for_running_path() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("load-partial");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        let handle = mark_path_running(&mut app, &path);
        app.running_partial_results
            .insert(path.clone(), vec!["r1".to_string()]);

        app.load_query_file(path).unwrap();

        assert_eq!(app.query_results, vec!["r1".to_string()]);
        assert_ne!(app.results_text, "Busy, check back later");

        handle.abort();
        let _ = app.rt.block_on(handle);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn completion_and_cancel_drop_partial_results() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("drop-partial");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.active_query_file = Some(path.clone());

        let handle = mark_path_running(&mut app, &path);
        app.running_partial_results
            .insert(path.clone(), vec!["r1".to_string()]);
        app.apply_query_completion(QueryCompletion {
            path: path.clone(),
            started_at: Instant::now(),
            outcome: Ok(vec!["done".to_string()]),
            total_runs: 2,
        });
        assert!(app.running_partial_results.is_empty());
        assert_eq!(app.query_results, vec!["done".to_string()]);

        let handle2 = mark_path_running(&mut app, &path);
        app.running_partial_results
            .insert(path.clone(), vec!["r1".to_string()]);
        app.cancel_running_query();
        assert!(app.running_partial_results.is_empty());

        handle.abort();
        let _ = app.rt.block_on(handle);
        handle2.abort();
        let _ = app.rt.block_on(handle2);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn multi_run_form_key_tabs_types_and_submits() {
        let mut app = test_app();
        app.mode = AppMode::MultiRunForm;
        app.multi_run_form_fields = vec!["3".to_string(), String::new()];

        handle_multi_run_form_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.multi_run_form_fields[0], "3R");

        handle_multi_run_form_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.multi_run_form_step, 1);

        handle_multi_run_form_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.multi_run_form_fields[1], "5");

        handle_multi_run_form_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.multi_run_form_fields[1], "");
    }

    #[test]
    fn execute_query_focuses_results_panel_for_the_run() {
        let mut app = test_app();
        // Point at a closed port so the background request fails fast.
        app.client = Some(
            MarkLogicClient::new(ServerConfig {
                name: "refused".to_string(),
                host: "127.0.0.1".to_string(),
                username: "admin".to_string(),
                password: "admin".to_string(),
                port: 9,
                secure: false,
                auth_type: AuthType::Digest,
                insecure: false,
                app_servers: vec![],
                uri: None,
            })
            .unwrap(),
        );
        let temp_dir = temp_test_dir("run-focus");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "1").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.active_query_file = Some(path.clone());
        app.query_files = vec![path.clone()];
        app.focus_query_panel();

        app.execute_query();

        assert!(app.running_queries.contains_key(&path));
        assert_eq!(app.focus, Focus::Results);

        for _ in 0..100 {
            app.poll_query_completions();
            if !app.running_queries.contains_key(&path) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!app.running_queries.contains_key(&path));

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn submit_multi_run_form_valid_starts_background_run() {
        let mut app = test_app();
        // Point at a closed port so the background request fails fast.
        app.client = Some(
            MarkLogicClient::new(ServerConfig {
                name: "refused".to_string(),
                host: "127.0.0.1".to_string(),
                username: "admin".to_string(),
                password: "admin".to_string(),
                port: 9,
                secure: false,
                auth_type: AuthType::Digest,
                insecure: false,
                app_servers: vec![],
                uri: None,
            })
            .unwrap(),
        );
        let temp_dir = temp_test_dir("multirun-submit");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "1").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.active_query_file = Some(path.clone());
        app.query_files = vec![path.clone()];
        app.mode = AppMode::MultiRunForm;
        app.multi_run_form_fields = vec!["2".to_string(), "0".to_string()];
        app.focus_query_panel();

        app.submit_multi_run_form();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.last_multi_run_count, 2);
        assert!(app.running_queries.contains_key(&path));
        assert_eq!(app.results_text, "Running multi-run (2 runs)...");
        assert_eq!(app.focus, Focus::Results);

        for _ in 0..100 {
            app.poll_query_completions();
            if !app.running_queries.contains_key(&path) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(!app.running_queries.contains_key(&path));
        assert_eq!(
            app.query_results.first().map(String::as_str),
            Some("--- MULTI-RUN #1 FAILED ---")
        );
        assert!(app.query_results.iter().any(|p| p.contains("Query error")));

        fs::remove_dir_all(temp_dir).unwrap();
    }

    /// App wired to a fresh query root with `.marklogic-tui` cache beside it.
    fn query_tree_app(name: &str) -> (App, PathBuf) {
        let root = temp_test_dir(name);
        fs::create_dir_all(&root).unwrap();
        let mut app = test_app();
        app.query_root_dir = root.clone();
        app.query_result_cache_dir = root.join(".marklogic-tui");
        (app, root)
    }

    fn write_query(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "xquery version \"1.0-ml\";").unwrap();
    }

    fn start_rename(app: &mut App, target: &Path, input: &str) {
        app.refresh_query_files().unwrap();
        app.rename_file_target = Some(target.to_path_buf());
        app.rename_file_input = input.to_string();
    }

    #[test]
    fn rename_file_to_nested_path_creates_folders() {
        let (mut app, root) = query_tree_app("rename-nested");
        let old = root.join("foo-bar.xqy");
        write_query(&old);
        app.active_query_file = Some(old.clone());
        start_rename(&mut app, &old, "foo/foo/foo/blah.xqy");

        app.rename_query_file();

        let new = root.join("foo/foo/foo/blah.xqy");
        assert!(new.is_file());
        assert!(!old.exists());
        assert_eq!(app.active_query_file, Some(new));
        // The root is never pruned.
        assert!(root.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_file_removes_source_folder_once_it_is_empty() {
        let (mut app, root) = query_tree_app("rename-prune");
        let old = root.join("old/a.xqy");
        write_query(&old);
        start_rename(&mut app, &old, "moved/a.xqy");

        app.rename_query_file();

        assert!(root.join("moved/a.xqy").is_file());
        assert!(!root.join("old").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_file_requires_supported_extension() {
        let (mut app, root) = query_tree_app("rename-ext");
        let old = root.join("a.xqy");
        write_query(&old);
        start_rename(&mut app, &old, "sub/a.txt");

        app.rename_query_file();

        assert!(old.is_file());
        assert!(!root.join("sub").exists());
        assert!(app.status_message.contains("supported extension"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_folder_moves_contents_cache_and_active_file() {
        let (mut app, root) = query_tree_app("rename-folder");
        let old_file = root.join("foo/a.xqy");
        write_query(&old_file);
        let cache = root.join(".marklogic-tui/foo/a.xqy");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("results-v1.json"), "{}").unwrap();
        app.active_query_file = Some(old_file);
        start_rename(&mut app, &root.join("foo"), "bar/baz");

        app.rename_query_file();

        assert!(root.join("bar/baz/a.xqy").is_file());
        assert!(!root.join("foo").exists());
        assert_eq!(app.active_query_file, Some(root.join("bar/baz/a.xqy")));
        assert!(
            root.join(".marklogic-tui/bar/baz/a.xqy/results-v1.json")
                .is_file()
        );
        assert!(!root.join(".marklogic-tui/foo").exists());
        assert_eq!(
            app.selected_query_row().map(|r| r.path),
            Some(root.join("bar/baz"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_folder_into_itself_is_rejected() {
        let (mut app, root) = query_tree_app("rename-self");
        let old_file = root.join("foo/a.xqy");
        write_query(&old_file);
        start_rename(&mut app, &root.join("foo"), "foo/inner");

        app.rename_query_file();

        assert!(old_file.is_file());
        assert!(app.status_message.contains("into itself"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_is_refused_while_a_query_under_the_path_is_running() {
        let (mut app, root) = query_tree_app("rename-running");
        let old_file = root.join("foo/a.xqy");
        write_query(&old_file);
        let handle = mark_path_running(&mut app, &old_file);
        start_rename(&mut app, &root.join("foo"), "bar");

        app.rename_query_file();

        handle.abort();
        assert!(root.join("foo/a.xqy").is_file());
        assert!(app.status_message.contains("running"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn create_accepts_nested_path_and_creates_folders() {
        let (mut app, root) = query_tree_app("create-nested");
        app.new_query_file_input = "sub/dir/new.xqy".to_string();

        app.create_new_query_file_from_input();

        assert!(root.join("sub/dir/new.xqy").is_file());
        assert_eq!(app.active_query_file, Some(root.join("sub/dir/new.xqy")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_prunes_empty_folders_and_their_cache() {
        let (mut app, root) = query_tree_app("refresh-prune");
        fs::create_dir_all(root.join("empty/inner")).unwrap();
        fs::create_dir_all(root.join(".marklogic-tui/empty/inner")).unwrap();

        app.refresh_query_files().unwrap();

        assert!(!root.join("empty").exists());
        assert!(!root.join(".marklogic-tui/empty").exists());
        assert!(app.query_folders.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folders_start_expanded_and_toggle_hides_children() {
        let (mut app, root) = query_tree_app("toggle");
        write_query(&root.join("docs/a.xqy"));
        app.refresh_query_files().unwrap();
        assert_eq!(app.query_file_rows().len(), 2);

        app.toggle_query_folder(&root.join("docs"));
        assert_eq!(app.query_file_rows().len(), 1);
        assert_eq!(
            app.selected_query_row().map(|r| r.path),
            Some(root.join("docs"))
        );

        app.toggle_query_folder(&root.join("docs"));
        assert_eq!(app.query_file_rows().len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enter_on_folder_row_toggles_without_loading_a_file() {
        let (mut app, root) = query_tree_app("enter-folder");
        let file = root.join("docs/a.xqy");
        write_query(&file);
        app.refresh_query_files().unwrap();
        app.focus_query_panel();
        app.file_list_visible = true;
        app.set_query_row_index(0);

        handle_query_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.collapsed_query_folders.contains(&root.join("docs")));
        assert_eq!(app.active_query_file, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_list_width_follows_widest_name_within_bounds() {
        let (mut app, root) = query_tree_app("width");
        assert_eq!(ui::sidebar_width(&app), 15);

        let short = root.join("q/a.xqy");
        write_query(&short);
        app.refresh_query_files().unwrap();
        assert_eq!(ui::sidebar_width(&app), 15);

        let long_name = format!("{}.xqy", "x".repeat(120));
        write_query(&root.join("q").join(&long_name));
        app.refresh_query_files().unwrap();
        assert_eq!(ui::sidebar_width(&app), 100);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn create_rejects_top_level_name() {
        let (mut app, root) = query_tree_app("create-loose");
        app.new_query_file_input = "loose.xqy".to_string();

        app.create_new_query_file_from_input();

        assert!(!root.join("loose.xqy").exists());
        assert!(app.status_message.contains("in a folder"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_file_to_top_level_name_is_rejected() {
        let (mut app, root) = query_tree_app("rename-loose");
        let old = root.join("q/a.xqy");
        write_query(&old);
        start_rename(&mut app, &old, "b.xqy");

        app.rename_query_file();

        assert!(old.is_file());
        assert!(!root.join("b.xqy").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_folder_to_top_level_name_is_allowed() {
        let (mut app, root) = query_tree_app("rename-folder-top");
        write_query(&root.join("q/a.xqy"));
        start_rename(&mut app, &root.join("q"), "renamed");

        app.rename_query_file();

        assert!(root.join("renamed/a.xqy").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_notice_lists_loose_top_level_files() {
        let (mut app, root) = query_tree_app("loose-notice");
        write_query(&root.join("loose.xqy"));
        app.refresh_query_files().unwrap();

        assert_eq!(app.loose_query_files, vec![root.join("loose.xqy")]);
        assert!(app.query_file_rows().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_completion_shows_no_output() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("empty-completion");
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("query-1.xqy");
        fs::write(&path, "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.active_query_file = Some(path.clone());
        let handle = mark_path_running(&mut app, &path);

        app.query_tx
            .send(QueryEvent::Completion(QueryCompletion {
                path: path.clone(),
                started_at: Instant::now(),
                outcome: Ok(Vec::new()),
                total_runs: 1,
            }))
            .unwrap();
        app.poll_query_completions();

        assert!(app.query_results.is_empty());
        assert_eq!(app.results_text, NO_OUTPUT_MESSAGE);

        handle.abort();
        let _ = app.rt.block_on(handle);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn cached_empty_results_show_no_output_when_reopened() {
        let (mut app, root) = query_tree_app("cached-no-output");
        let path = root.join("q/a.xqy");
        write_query(&path);
        save_query_result_snapshot(
            &app.query_result_cache_dir,
            &app.query_root_dir,
            &path,
            &QueryResultSnapshot {
                query_results: Vec::new(),
                selected_index: None,
                created_at: Some(1),
            },
        )
        .unwrap();

        app.load_query_file(path).unwrap();

        assert!(app.query_results.is_empty());
        assert_eq!(app.results_text, NO_OUTPUT_MESSAGE);
        fs::remove_dir_all(root).unwrap();
    }

    fn find_rendered_text(buf: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
        let area = buf.area;
        for y in area.top()..area.bottom() {
            let line: String = (area.left()..area.right())
                .filter_map(|x| buf.cell((x, y)).map(|cell| cell.symbol().to_string()))
                .collect();
            if let Some(byte_index) = line.find(needle) {
                let column = line[..byte_index].chars().count() as u16;
                return Some((area.left() + column, y));
            }
        }
        None
    }

    #[test]
    fn render_colors_no_output_yellow_and_dims_folder_names() {
        let (mut app, root) = query_tree_app("render-style");
        write_query(&root.join("q/a.xqy"));
        app.refresh_query_files().unwrap();
        app.query_visible = true;
        app.file_list_visible = true;
        app.focus_query_panel();
        // Select the file row: the selection highlight would otherwise
        // override the folder's own colour.
        app.query_file_list_state.select(Some(1));
        app.query_results.clear();
        app.results_text = NO_OUTPUT_MESSAGE.to_string();

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let (x, y) = find_rendered_text(&buf, "No output").expect("No output is rendered");
        assert_eq!(buf.cell((x, y)).unwrap().fg, Color::Yellow);

        let (x, y) = find_rendered_text(&buf, "▾ q").expect("folder row is rendered");
        assert_eq!(buf.cell((x + 2, y)).unwrap().fg, Color::DarkGray);
        assert!(
            !buf.cell((x + 2, y))
                .unwrap()
                .modifier
                .contains(Modifier::BOLD)
        );

        fs::remove_dir_all(root).unwrap();
    }

    /// Query root with edtui as the editor and the Files pane visible.
    #[test]
    fn screen_shortcuts_toggle_history_and_preserve_query_and_logs() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.show_query_editor();
        app.set_edit_mode(EditMode::Insert);
        app.query_editor.insert_str("unsaved query");
        app.query_dirty = true;
        press(&mut app, KeyCode::Char('2'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        app.server_logs.scroll = 42;
        app.server_logs.search = "incident".into();
        press(&mut app, KeyCode::Char('.'), KeyModifiers::CONTROL);
        assert_eq!(app.current_screen(), Screen::Query);
        assert_eq!(app.edit_mode, EditMode::Insert);
        assert!(
            app.query_editor
                .lines()
                .join("\n")
                .contains("unsaved query")
        );
        assert!(app.query_dirty);
        press(&mut app, KeyCode::Char('.'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        assert_eq!(app.server_logs.scroll, 42);
        assert_eq!(app.server_logs.search, "incident");
        press(&mut app, KeyCode::Char('3'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, AppMode::Reports);
        press(&mut app, KeyCode::Char('.'), KeyModifiers::ALT);
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        press(&mut app, KeyCode::Char('.'), KeyModifiers::ALT);
        assert_eq!(app.mode, AppMode::Reports);
        press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!(app.current_screen(), Screen::Query);
        assert_eq!(app.edit_mode, EditMode::Insert);
    }

    #[test]
    fn switching_to_documents_does_not_discard_query_results() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.show_query_editor();
        app.set_query_results(vec!["important query result".into()]);
        press(&mut app, KeyCode::Char('5'), KeyModifiers::CONTROL);
        assert_eq!(app.current_screen(), Screen::Documents);
        assert_eq!(app.query_results, vec!["important query result"]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("Use :list to load documents"));
        assert!(!content.contains("important query result"));
        press(&mut app, KeyCode::Char('.'), KeyModifiers::CONTROL);
        assert_eq!(app.current_screen(), Screen::Query);
        assert_eq!(app.query_results, vec!["important query result"]);
    }

    #[test]
    fn screen_shortcuts_keep_open_forms_and_navigation_history() {
        let mut app = test_app();
        app.mode = AppMode::ServerForm;
        app.server_form_fields[0] = "draft server".into();
        app.server_form_step = 1;
        press(&mut app, KeyCode::Char('1'), KeyModifiers::CONTROL);
        assert_eq!(app.current_screen(), Screen::Query);
        press(&mut app, KeyCode::Char('4'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, AppMode::ServerForm);
        assert_eq!(app.server_form_fields[0], "draft server");
        assert_eq!(app.server_form_step, 1);
        // Selecting the already active screen must not change the previous one.
        press(&mut app, KeyCode::Char('4'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('.'), KeyModifiers::CONTROL);
        assert_eq!(app.current_screen(), Screen::Query);
    }

    #[test]
    fn command_navigation_records_previous_screen_and_consumes_the_command() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.show_query_editor();
        app.focus_command_panel();
        app.command_input = ":logs".into();
        app.autocomplete_suggestions.clear();
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
        press(&mut app, KeyCode::Char('.'), KeyModifiers::CONTROL);
        assert_eq!(app.current_screen(), Screen::Query);
        assert_eq!(app.focus, Focus::Query);
        assert!(app.command_input.is_empty());
        press(&mut app, KeyCode::Char('.'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, AppMode::MarkLogicLogs);
    }

    fn edtui_query_app(name: &str) -> (App, PathBuf) {
        let (mut app, root) = query_tree_app(name);
        app.use_edtui = true;
        app.query_visible = true;
        app.file_list_visible = true;
        (app, root)
    }

    fn write_empty(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap();
    }

    #[test]
    fn inline_editor_flag_defaults_to_edtui() {
        assert!(inline_editor_uses_edtui(None).unwrap());
        assert!(inline_editor_uses_edtui(Some("edtui")).unwrap());
        assert!(!inline_editor_uses_edtui(Some("textarea")).unwrap());
        assert!(inline_editor_uses_edtui(Some("vim")).is_err());
    }

    #[test]
    fn edtui_insert_mode_types_keys_that_are_shortcuts_in_normal_mode() {
        let (mut app, root) = edtui_query_app("edtui-insert-types");
        let file = root.join("q/a.xqy");
        write_empty(&file);
        app.refresh_query_files().unwrap();
        app.select_query_file(file);

        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(app.edit_mode, EditMode::Insert);
        assert_eq!(app.edtui_state.mode, EditorMode::Insert);

        for c in ['1', '\\', '?'] {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.query_text(), "1\\?");
        assert!(
            app.file_list_visible,
            "backslash must not toggle the Files pane"
        );
        assert_eq!(app.mode, AppMode::Normal, "? must not open help");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_escape_returns_to_normal_mode() {
        let (mut app, root) = edtui_query_app("edtui-escape");
        let file = root.join("q/a.xqy");
        write_empty(&file);
        app.refresh_query_files().unwrap();
        app.select_query_file(file);

        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert_eq!(app.edtui_state.mode, EditorMode::Normal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_visual_mode_counts_as_insert_for_app_keys() {
        let (mut app, root) = edtui_query_app("edtui-visual");
        let file = root.join("q/a.xqy");
        write_empty(&file);
        app.refresh_query_files().unwrap();
        app.select_query_file(file);

        press(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(app.edit_mode, EditMode::Insert);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_append_and_open_line_enter_insert_mode() {
        for key in ['a', 'o'] {
            let (mut app, root) = edtui_query_app("edtui-insert-keys");
            let file = root.join("q/a.xqy");
            write_empty(&file);
            app.refresh_query_files().unwrap();
            app.select_query_file(file);

            press(&mut app, KeyCode::Char(key), KeyModifiers::NONE);

            assert_eq!(app.edit_mode, EditMode::Insert, "key {key}");
            assert_eq!(app.edtui_state.mode, EditorMode::Insert, "key {key}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn edtui_normal_mode_j_moves_files_selection_and_opens_file() {
        let (mut app, root) = edtui_query_app("edtui-j");
        let first = root.join("q/a.xqy");
        let second = root.join("q/b.xqy");
        write_empty(&first);
        write_empty(&second);
        app.refresh_query_files().unwrap();
        app.select_query_file(first);
        // Rows: folder q, a.xqy, b.xqy.
        app.query_file_list_state.select(Some(1));

        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);

        assert_eq!(app.query_file_list_state.selected(), Some(2));
        assert_eq!(app.active_query_file, Some(second));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_normal_mode_enter_folds_folder_row() {
        let (mut app, root) = edtui_query_app("edtui-enter");
        write_empty(&root.join("docs/a.xqy"));
        app.refresh_query_files().unwrap();
        app.focus_query_panel();
        app.query_file_list_state.select(Some(0));

        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert!(app.collapsed_query_folders.contains(&root.join("docs")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_normal_mode_passes_editing_keys_to_editor_when_files_pane_hidden() {
        let (mut app, root) = edtui_query_app("edtui-hidden-pane");
        app.file_list_visible = false;
        app.edtui_state = EditorState::new(Lines::from("abc"));
        app.focus_query_panel();

        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);

        assert_eq!(app.query_text(), "bc");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_run_shortcut_works_in_insert_mode() {
        let (mut app, root) = edtui_query_app("edtui-run");
        let file = root.join("q/a.xqy");
        write_empty(&file);
        app.refresh_query_files().unwrap();
        app.select_query_file(file);
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);

        press(&mut app, KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(app.results_text, "No server connected.");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_renders_xquery_keywords_with_highlight_colours() {
        let (mut app, root) = edtui_query_app("edtui-highlight");
        let file = root.join("q/a.xqy");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "declare variable $DRY := true();").unwrap();
        app.refresh_query_files().unwrap();
        app.select_query_file(file);

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let (x, y) = find_rendered_text(&buf, "declare").expect("keyword is rendered");
        let fg = buf.cell((x, y)).unwrap().fg;
        assert!(
            matches!(fg, Color::Rgb(..)),
            "expected a highlight colour, got {fg:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edtui_draws_editor_once_with_one_colour_per_word_and_its_own_gutter() {
        let (mut app, root) = edtui_query_app("edtui-render-once");
        let file = root.join("q/a.xqy");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            "declare variable $DRY := true();\nlet $x := 1\nreturn $x",
        )
        .unwrap();
        app.refresh_query_files().unwrap();
        app.select_query_file(file);
        // The block cursor is painted with inverted colours. Park it on the
        // variable at the end of the last line, away from the keywords checked.
        app.edtui_state.cursor = edtui::Index2::new(2, 8);

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| ui::ui(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // The hidden textarea used to be drawn over edtui. Its title gives it away.
        assert!(
            find_rendered_text(&buf, "Alt+Enter").is_none(),
            "textarea title drawn over the edtui editor"
        );

        // The file text appears once, not as two overlapping copies.
        let rows_with_keyword = (buf.area.top()..buf.area.bottom())
            .filter(|&y| row_text(&buf, y).contains("declare"))
            .count();
        assert_eq!(rows_with_keyword, 1);

        // Every word has one colour across all of its cells.
        for word in ["declare", "variable", "let", "return"] {
            let (x, y) =
                find_rendered_text(&buf, word).unwrap_or_else(|| panic!("{word} is rendered"));
            let colours: std::collections::HashSet<Color> = (0..word.len() as u16)
                .map(|i| buf.cell((x + i, y)).unwrap().fg)
                .collect();
            assert_eq!(colours.len(), 1, "{word} has mixed colours: {colours:?}");
            assert!(matches!(colours.iter().next(), Some(Color::Rgb(..))));
        }

        // The line number sits in the gutter, left of the text on its own row.
        let (x, y) = find_rendered_text(&buf, "declare").expect("keyword is rendered");
        let row = row_text(&buf, y);
        let before: String = row.chars().take(x as usize).collect();
        assert!(before.contains('1'), "gutter number missing: {row:?}");

        fs::remove_dir_all(root).unwrap();
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        let area = buf.area;
        (area.left()..area.right())
            .filter_map(|x| buf.cell((x, y)).map(|cell| cell.symbol().to_string()))
            .collect()
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

fn init_file_logging(level: system::LogLevel) -> Result<system::LogFilterHandle> {
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

    use tracing_subscriber::prelude::*;
    let (filter, handle) = tracing_subscriber::reload::Layer::new(level.filter());
    let output = tracing_subscriber::fmt::layer()
        .with_writer(move || {
            log_file
                .try_clone()
                .expect("failed to clone log file handle")
        })
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true);
    let subscriber = tracing_subscriber::registry().with(filter).with(output);
    tracing::subscriber::set_global_default(subscriber)?;
    debug!(path = %log_path.display(), "logging initialized");

    Ok(handle)
}

/// Which inline editor to use. edtui is the default. `textarea` selects the
/// simpler editor, which has no syntax highlighting.
fn inline_editor_uses_edtui(value: Option<&str>) -> Result<bool> {
    match value {
        None | Some("edtui") => Ok(true),
        Some("textarea") => Ok(false),
        Some(other) => anyhow::bail!("unknown inline editor '{other}': use edtui or textarea"),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Validate an explicit CLI override before importing or exporting config.
    let editor_override = args
        .inline_editor
        .as_deref()
        .map(|value| inline_editor_uses_edtui(Some(value)))
        .transpose()?;

    if let Some(path) = &args.export_config {
        let bundle = config::ConfigBundle::current()?;
        bundle.export_to(path)?;
        println!(
            "Exported configuration to {}\nNote: server passwords are written in plaintext.",
            path.display()
        );
        return Ok(());
    }

    if let Some(path) = &args.config {
        config::ConfigBundle::import_from(path)?;
        println!("Imported configuration from {}", path.display());
    }

    let settings = AppConfig::load()?.system;
    let use_edtui =
        editor_override.unwrap_or(settings.inline_editor == system::InlineEditor::Edtui);
    let log_filter = init_file_logging(settings.log_level)?;
    info!(use_edtui, "starting marklogic-tui");

    enable_raw_mode()?;
    keyboard::enable()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableFocusChange,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(use_edtui, args.config.clone())?;
    app.system.log_filter = Some(log_filter);

    loop {
        if app.needs_terminal_refresh {
            terminal.clear()?;
            app.needs_terminal_refresh = false;
        }
        terminal.draw(|f| ui::ui(f, &mut app))?;
        if events::handle_event(&mut app)? {
            break;
        }
        app.poll_query_completions();
        app.reconcile_report_source();
        app.reports.poll();
        app.server_logs.reconcile_client(app.client.as_ref());
        app.server_logs
            .tick(&app.rt, app.mode == AppMode::MarkLogicLogs);
        app.maybe_autosave();
        app.clear_expired_status_message();
        app.log_status_message_if_changed();
    }

    keyboard::disable(terminal.backend_mut())?;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableFocusChange,
        LeaveAlternateScreen
    )?;
    info!("shutting down marklogic-tui");
    Ok(())
}
