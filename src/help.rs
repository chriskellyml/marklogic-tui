use crate::{AppInterface, AppMode, EditMode, Focus};

/// A single keybinding. `desc` is the full help-overlay description; `bar` is
/// the short label shown in the top keybindings bar (`None` = not shown there).
pub(crate) struct Binding {
    pub key: &'static str,
    pub desc: &'static str,
    pub bar: Option<&'static str>,
}

const fn b(key: &'static str, desc: &'static str) -> Binding {
    Binding {
        key,
        desc,
        bar: None,
    }
}

const fn bb(key: &'static str, desc: &'static str, bar: &'static str) -> Binding {
    Binding {
        key,
        desc,
        bar: Some(bar),
    }
}

pub(crate) struct HelpSection {
    pub title: &'static str,
    pub bindings: &'static [Binding],
}

const fn section(title: &'static str, bindings: &'static [Binding]) -> HelpSection {
    HelpSection { title, bindings }
}

const GLOBAL: &[Binding] = &[
    b("?", "Show/hide this help"),
    b("F2", "Open application logs"),
    b(":", "Open command input"),
    b("1", "Focus query panel"),
    b("2", "Focus results panel"),
    b("3", "Focus command panel"),
    b("\\", "Toggle file list"),
    b("Tab", "Cycle focus forward"),
    b("Shift+Tab", "Cycle focus backward"),
];

const COMMAND_PANEL: &[Binding] = &[
    bb("Enter", "Execute command", "Execute"),
    bb("Tab", "Accept autocomplete / cycle panels", "Complete"),
    b("Up/Down", "Navigate autocomplete"),
    bb("Esc", "Close command line", "Close"),
];

const QUERY_INSERT: &[Binding] = &[
    bb("Esc", "Return to normal mode", "Normal"),
    b("Ctrl+T", "Cycle focus"),
    bb("Ctrl+R", "Override declared variables", "Variables"),
    b("F5", "Run query"),
    bb("Ctrl+V", "Edit active variables", "Variables"),
    bb("Ctrl+R", "Override declared variables", "Variables"),
    bb("Ctrl+S", "Save query file", "Save"),
    b("Ctrl+O", "Switch query file"),
];

const QUERY_NAV: &[Binding] = &[
    bb("i", "Enter insert mode (editor)", "Insert"),
    b("j/k", "Navigate + auto-load file"),
    b("Enter", "Fold folder / load selected file"),
    bb("T", "Toggle Files/Modules sidebox", "Files↔Mods"),
    bb("m", "Open module search", "Modules"),
    bb("n", "New query file", "New"),
    bb("e", "Open in external editor", "Editor"),
    b("p", "Preview compiled query (imports and variables resolved) in external editor"),
    bb("Ctrl+V", "Edit active variables", "Variables"),
    bb("Ctrl+R", "Override declared variables", "Variables"),
    bb("r", "Run query", "Run"),
    b("F5", "Run query"),
    bb("R", "Multi-run query", "Multi-run"),
    bb("X", "Cancel running query", "Cancel"),
    b("\\", "Toggle file list"),
    b("g/G", "Go to top/bottom of file list"),
];

const QUERY_FILES: &[Binding] = &[
    bb("f", "Open tracked folders", "Folders"),
    bb("M", "Rename selected file", "Rename"),
    b("Delete", "Delete selected file"),
];

/// Query-panel base bindings when the Modules sidebox is active: `n` and
/// `r` belong to the sidebox there, so they move to [`QUERY_MODULES`].
const QUERY_NAV_MODULES: &[Binding] = &[
    bb("i", "Enter insert mode (editor)", "Insert"),
    b("j/k", "Navigate + auto-load file"),
    b("Enter", "Fold folder / load selected file"),
    bb("T", "Toggle Files/Modules sidebox", "Files↔Mods"),
    bb("m", "Open module search", "Modules"),
    bb("e", "Open in external editor", "Editor"),
    b("p", "Preview compiled query (imports and variables resolved) in external editor"),
    bb("Ctrl+V", "Edit active variables", "Variables"),
    bb("F5", "Run query (r is reset in Modules view)", "Run"),
    bb("R", "Multi-run query", "Multi-run"),
    bb("X", "Cancel running query", "Cancel"),
    b("\\", "Toggle file list"),
    b("g/G", "Go to top/bottom of module tree"),
];

const QUERY_MODULES: &[Binding] = &[
    bb("s", "Sync all unsynced modules", "Sync"),
    bb(
        "r",
        "Reset selected from the active server version",
        "Reset",
    ),
    bb("n", "New module (enter URI first)", "New"),
    bb("D", "Drop local cached copy (confirm)", "Drop"),
    bb("d", "Diff original vs working copy", "Diff"),
];

const QUERY_SYNC: &[Binding] = &[
    bb("s", "Sync module to server", "Sync"),
    bb("t", "Reset remote module to original", "Reset"),
    bb("d", "Drop local module checkout", "Drop"),
];

const RESULTS: &[Binding] = &[
    bb("Enter", "Open selected document", "Open"),
    bb("F", "Choose query result format", "Format"),
    b("j/k", "Navigate up/down"),
    b("d/u", "Page down/up"),
    bb("n / p", "Next/previous page", "Page"),
    bb("Space", "Toggle selection", "Select"),
    bb("/", "Filter visible query results (* and ?)", "Filter"),
    bb("c", "Create new document", "Create"),
    bb("Ctrl+D", "Delete selected document or URI", "Delete"),
    bb("X", "Cancel running query", "Cancel"),
    b("g", "Go to top (double-tap)"),
    b("G", "Go to bottom"),
];

const FILTER: &[Binding] = &[
    bb("Enter", "Apply filter", "Apply"),
    bb("Esc", "Cancel filter", "Cancel"),
];

const SERVERS: &[Binding] = &[
    b("Tab", "Switch between servers and app servers"),
    b("Enter", "Activate selected server/app server"),
    b("a", "Add server (server focus)"),
    b("e", "Edit selected server (server focus)"),
    b("d", "Remove selected server (server focus)"),
    b("r", "Refresh app servers (app-server focus)"),
    b("p", "Manage app servers (app-server focus)"),
    b("j/k", "Move selection down/up"),
    b("g/G", "Go to top/bottom"),
    b("q/Esc", "Close servers interface"),
];

const FORM: &[Binding] = &[
    b("Tab", "Next field"),
    b("Shift+Tab", "Previous field"),
    b("Enter", "Save"),
    b("Esc", "Cancel"),
];

const APP_SERVER_LIST: &[Binding] = &[
    b("Enter", "Activate selected app server"),
    b("a", "Add app server"),
    b("e", "Edit app server"),
    b("d", "Remove app server"),
    b("r", "Refresh from discovery"),
    b("Up/Down", "Move selection"),
    b("Home/End", "First/last"),
    b("q/Esc", "Back to servers"),
];

const COLLECTION_SELECT: &[Binding] = &[
    b("j/k", "Move selection"),
    b("Enter", "Select collection"),
    b("Esc", "Close"),
];

const QUERY_RESULT_FORMAT_SELECT: &[Binding] = &[
    b("j/k", "Move selection"),
    b("Enter", "Apply selected format"),
    b("Esc", "Cancel"),
];

const ACTIVE_VARIABLES: &[Binding] = &[
    b("Tab/Shift+Tab", "Move between name and value fields"),
    b("Ctrl+N", "Add variable"),
    b("Delete", "Remove variable"),
    b("Enter", "Save variables"),
    b("Esc", "Cancel"),
];

const DECLARED_VARIABLES: &[Binding] = &[
    b("Tab/Shift+Tab", "Move between variable values"),
    b("Enter", "Run query with overrides"),
    b("Esc", "Cancel"),
];

const QUERY_FILE_SELECT: &[Binding] = &[
    b("j/k", "Move selection"),
    b("Enter", "Fold folder / open file"),
    b("Esc", "Close"),
];

const MULTI_RUN_FORM: &[Binding] = &[
    b("Tab/Shift+Tab", "Cycle fields"),
    b("Enter", "Start multi-run"),
    b("Esc", "Cancel"),
];

const MODULE_CLONE: &[Binding] = &[
    b("Tab", "Switch between search input and results"),
    b("Enter", "Search (input) / download marked (list)"),
    b("Space", "Mark/unmark highlighted module"),
    b("a", "Mark all visible"),
    b("u", "Clear marks"),
    b("S", "Download marked modules"),
    b("n/p", "Next/previous page"),
    b("g/G", "First/last result"),
    b("Esc", "Close"),
];

const MODULE_CREATE: &[Binding] = &[b("Enter", "Create module and load it"), b("Esc", "Cancel")];

const MODULE_DIFF: &[Binding] = &[
    b("j/k", "Scroll down/up"),
    b("Space/PageDown", "Page down"),
    b("PageUp", "Page up"),
    b("g/G", "Top/bottom"),
    b("q/Esc", "Close"),
];

const TRACKED_FOLDERS: &[Binding] = &[
    b("j/k", "Move selection"),
    b("g/G", "First/last"),
    b("Enter", "Switch to folder"),
    b("a", "Add folder"),
    b("f", "Toggle favorite"),
    b("d", "Delete folder"),
    b("c", "Clear cache"),
    b("Esc", "Close"),
];

const CONFIRM: &[Binding] = &[bb("y", "Confirm", "Confirm"), b("n/Esc", "Cancel")];

const MODULE_RESET_CONFIRM: &[Binding] = &[
    b("y", "Reset, keep local checkout"),
    b("d", "Reset and drop local checkout"),
    b("n/Esc", "Cancel"),
];

const FULLSCREEN: &[Binding] = &[
    b("Esc/q", "Close document view"),
    b("e", "Edit in external editor"),
    b("m", "Edit document (full-screen form)"),
    b("j/k", "Scroll down/up"),
    b("d/u", "Page down/up"),
    b("Space", "Page down"),
    b("PageUp", "Page up"),
    b("g/G", "Top/bottom"),
];

const SERVER_LOGS: &[Binding] = &[
    b("Esc/q", "Close MarkLogic logs"),
    b("m", "Switch selected-file / combined tracked-log view"),
    b("\\", "Hide / show the log-file sidebar"),
    b(
        "v",
        "Combined display: raw, relative time, XML details, wrap, search and log types",
    ),
    b(
        "t",
        "Seek combined logs to a server timestamp (HH:MM:SS or full date/time)",
    ),
    b("+/-", "Jump forward/backward one minute in combined logs"),
    b("n/N", "Next/previous matching record in combined logs"),
    b("e/E", "Next/previous error record in combined logs"),
    b(
        "S",
        "Save the visible, formatted/filtered log view to the cache directory",
    ),
    b("V", "Open the current view in $EDITOR"),
    b("Tab", "Switch files / content focus"),
    b("j/k", "Select file or scroll content"),
    b(
        "F",
        "With files focused: discover current-day logs and filter the list",
    ),
    b(
        "c",
        "Clear the file-list filter or content search for the focused pane",
    ),
    b(
        "Enter/d",
        "Fetch selected file while the files pane is focused",
    ),
    b(
        "o",
        "Track / untrack the selected log on every host (persisted)",
    ),
    b(
        "b",
        "Download full selected file (reads the whole file on the server)",
    ),
    b("r", "Refresh cluster / log inventory"),
    b("s", "Catch up all tracked logs now"),
    b("D", "Cycle history window: today, 2, 7, 30 days"),
    b(
        "C",
        "Clear cached log content, reset to today, and sync again",
    ),
    b(
        "p",
        "Cycle selected port; in combined content open the port selection window",
    ),
    b("i", "Show only ports marked include (+)"),
    b("P", "Clear all port marks"),
    b("a", "Toggle searching across all cached logs"),
    b("f", "Toggle following: poll tracked logs every 5 seconds"),
    b("x", "Cancel download and pause following"),
    b(
        "/",
        "Search log content; combined mode accepts OR patterns separated by | and /regex/i",
    ),
    b("h/l", "Scroll horizontally"),
    b("g/G", "Top/bottom"),
    b("d/u", "Page content down/up"),
    b("PageUp/PageDown", "Scroll content one page"),
];

const LOG_VIEWER: &[Binding] = &[
    b("Esc/q", "Close log viewer"),
    b("e", "Open log in external editor"),
    b("j/k", "Scroll down/up"),
    b("d/u", "Page down/up"),
    b("Space", "Page down"),
    b("PageUp", "Page up"),
    b("g/G", "Top/bottom"),
];

const DOCUMENT_FORM: &[Binding] = &[
    b("Tab", "Next field"),
    b("Backspace", "Delete character"),
    b("Enter", "Save document"),
    b("Esc", "Cancel"),
];

const DOCUMENT_FIELDS: &[Binding] = &[
    b("URI", "Document URI (required)"),
    b("Collections", "Comma-separated list"),
    b("Quality", "Document quality (integer)"),
    b("Content", "Document body"),
];

/// Help sections for the given context. `mode` should be the *originating*
/// mode (when the help overlay is open, `App::previous_mode`), not
/// `AppMode::HelpOverlay`.
pub(crate) fn sections_for_context(
    mode: &AppMode,
    focus: &Focus,
    edit_mode: &EditMode,
    file_list_visible: bool,
    is_synced_module: bool,
    modules_view: bool,
) -> Vec<HelpSection> {
    match mode {
        AppMode::Interface(AppInterface::Servers)
        | AppMode::ServerForm
        | AppMode::ServerDeleteConfirm => {
            vec![
                section("Servers Interface", SERVERS),
                section("Server Form", FORM),
            ]
        }
        AppMode::AppServerList => vec![section("App Servers", APP_SERVER_LIST)],
        AppMode::AppServerForm => vec![section("App Server Form", FORM)],
        AppMode::AppServerDeleteConfirm
        | AppMode::DeleteConfirm
        | AppMode::QueryFileDeleteConfirm
        | AppMode::ModuleDropConfirm
        | AppMode::ModuleDropCacheConfirm
        | AppMode::TrackedFolderDeleteConfirm
        | AppMode::TrackedFolderCacheClearConfirm => vec![section("Confirm", CONFIRM)],
        AppMode::ModuleResetConfirm => vec![section("Confirm Module Reset", MODULE_RESET_CONFIRM)],
        AppMode::CollectionSelect => vec![section("Collection Select", COLLECTION_SELECT)],
        AppMode::QueryResultFormatSelect => {
            vec![section("Query Result Format", QUERY_RESULT_FORMAT_SELECT)]
        }
        AppMode::ActiveVariables => vec![section("Active Variables", ACTIVE_VARIABLES)],
        AppMode::DeclaredVariables => vec![section("Declared Variables", DECLARED_VARIABLES)],
        AppMode::QueryFileSelect => vec![section("Query File Select", QUERY_FILE_SELECT)],
        AppMode::QueryFileCreate | AppMode::QueryFileRename => {
            vec![section("Query File Form", FORM)]
        }
        AppMode::ModuleCreate => vec![section("New Module", MODULE_CREATE)],
        AppMode::MultiRunForm => vec![section("Multi-Run Query", MULTI_RUN_FORM)],
        AppMode::ModuleCloneSelect => vec![section("Module Search", MODULE_CLONE)],
        AppMode::ModuleDiffView => vec![section("Module Diff", MODULE_DIFF)],
        AppMode::TrackedFolderSelect => vec![section("Tracked Folders", TRACKED_FOLDERS)],
        AppMode::TrackedFolderAdd => vec![section("Add Tracked Folder", FORM)],
        AppMode::FullScreenView => vec![section("Document View", FULLSCREEN)],
        AppMode::LogViewer => vec![section("Log Viewer", LOG_VIEWER)],
        AppMode::MarkLogicLogs => vec![section("MarkLogic Logs", SERVER_LOGS)],
        AppMode::DocumentCreate | AppMode::DocumentMetadataEdit => vec![
            section("Document Form", DOCUMENT_FORM),
            section("Fields", DOCUMENT_FIELDS),
        ],
        AppMode::StartPage | AppMode::Normal | AppMode::HelpOverlay => {
            let mut sections = vec![section("Global", GLOBAL)];
            sections.extend(focus_sections(
                focus,
                edit_mode,
                file_list_visible,
                is_synced_module,
                modules_view,
            ));
            sections
        }
    }
}

fn focus_sections(
    focus: &Focus,
    edit_mode: &EditMode,
    file_list_visible: bool,
    is_synced_module: bool,
    modules_view: bool,
) -> Vec<HelpSection> {
    match focus {
        Focus::Command => vec![section("Command Panel", COMMAND_PANEL)],
        Focus::Query => {
            if *edit_mode == EditMode::Insert {
                vec![section("Query Panel (Insert Mode)", QUERY_INSERT)]
            } else {
                let mut sections = if modules_view {
                    vec![section("Query Panel (Normal Mode)", QUERY_NAV_MODULES)]
                } else {
                    vec![section("Query Panel (Normal Mode)", QUERY_NAV)]
                };
                if file_list_visible {
                    sections.push(if modules_view {
                        section("Modules List", QUERY_MODULES)
                    } else {
                        section("File List", QUERY_FILES)
                    });
                }
                if is_synced_module {
                    sections.push(section("Module Sync", QUERY_SYNC));
                }
                sections
            }
        }
        Focus::Results => vec![section("Results Panel", RESULTS)],
        Focus::Filter => vec![section("Filter Panel", FILTER)],
    }
}

/// Compact `(key, label)` items for the top keybindings bar in Normal mode.
pub(crate) fn bar_items(
    focus: &Focus,
    edit_mode: &EditMode,
    file_list_visible: bool,
    is_synced_module: bool,
    modules_view: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut items = Vec::new();
    for s in focus_sections(
        focus,
        edit_mode,
        file_list_visible,
        is_synced_module,
        modules_view,
    ) {
        for binding in s.bindings {
            if let Some(label) = binding.bar {
                items.push((binding.key, label));
            }
        }
    }
    items.push(("?", "Help"));
    items
}

/// Shorten a binding key for the narrow top bar: `Ctrl+R` -> `^R`,
/// `n / p` -> `n/p`.
pub(crate) fn bar_key(key: &str) -> String {
    key.replace("Ctrl+", "^").replace(' ', "")
}

pub(crate) fn render_help_lines(sections: &[HelpSection]) -> Vec<String> {
    let mut lines = vec![String::new()];
    for s in sections {
        lines.push(format!("  {}", s.title));
        let width = s
            .bindings
            .iter()
            .map(|b| b.key.chars().count())
            .max()
            .unwrap_or(0);
        for b in s.bindings {
            lines.push(format!("    {:<width$}  {}", b.key, b.desc, width = width));
        }
        lines.push(String::new());
    }
    lines.push("  Press ? or Esc to close this help".to_string());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(mode: &AppMode) -> Vec<&'static str> {
        sections_for_context(
            mode,
            &Focus::Results,
            &EditMode::Navigate,
            false,
            false,
            false,
        )
        .iter()
        .map(|s| s.title)
        .collect()
    }

    #[test]
    fn fullscreen_context_shows_document_view() {
        assert_eq!(titles(&AppMode::FullScreenView), vec!["Document View"]);
    }

    #[test]
    fn log_viewer_context_shows_log_viewer() {
        assert_eq!(titles(&AppMode::LogViewer), vec!["Log Viewer"]);
    }

    #[test]
    fn servers_context_shows_servers_interface() {
        let titles = titles(&AppMode::Interface(AppInterface::Servers));
        assert!(titles.contains(&"Servers Interface"));
    }

    #[test]
    fn normal_context_shows_global_and_focus_sections() {
        let sections = sections_for_context(
            &AppMode::Normal,
            &Focus::Query,
            &EditMode::Navigate,
            true,
            true,
            false,
        );
        let titles: Vec<_> = sections.iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            vec![
                "Global",
                "Query Panel (Normal Mode)",
                "File List",
                "Module Sync"
            ]
        );
    }

    #[test]
    fn modules_view_swaps_file_list_for_modules_list() {
        let sections = sections_for_context(
            &AppMode::Normal,
            &Focus::Query,
            &EditMode::Navigate,
            true,
            false,
            true,
        );
        let titles: Vec<_> = sections.iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            vec!["Global", "Query Panel (Normal Mode)", "Modules List"]
        );
        let items = bar_items(&Focus::Query, &EditMode::Navigate, true, false, true);
        assert!(items.contains(&("s", "Sync")));
        assert!(items.contains(&("r", "Reset")));
        assert!(items.contains(&("n", "New")));
        assert!(items.contains(&("D", "Drop")));
        assert!(!items.contains(&("r", "Run")));
        assert!(!items.contains(&("M", "Rename")));
    }

    #[test]
    fn files_view_bar_keeps_run_and_rename() {
        let items = bar_items(&Focus::Query, &EditMode::Navigate, true, false, false);
        assert!(items.contains(&("r", "Run")));
        assert!(items.contains(&("n", "New")));
        assert!(items.contains(&("M", "Rename")));
    }

    #[test]
    fn modal_contexts_have_dedicated_sections() {
        assert_eq!(titles(&AppMode::ModuleCloneSelect), vec!["Module Search"]);
        assert_eq!(titles(&AppMode::ModuleDiffView), vec!["Module Diff"]);
        assert_eq!(titles(&AppMode::ModuleCreate), vec!["New Module"]);
        assert_eq!(titles(&AppMode::ModuleDropCacheConfirm), vec!["Confirm"]);
        assert_eq!(
            titles(&AppMode::TrackedFolderSelect),
            vec!["Tracked Folders"]
        );
        assert_eq!(titles(&AppMode::QueryFileSelect), vec!["Query File Select"]);
        assert_eq!(titles(&AppMode::DeleteConfirm), vec!["Confirm"]);
    }

    #[test]
    fn bar_items_come_from_same_tables() {
        let items = bar_items(&Focus::Results, &EditMode::Navigate, false, false, false);
        assert!(items.contains(&("Enter", "Open")));
        assert!(items.contains(&("X", "Cancel")));
        assert_eq!(items.last(), Some(&("?", "Help")));
    }

    #[test]
    fn bar_key_shortens_modifiers() {
        assert_eq!(bar_key("Ctrl+D"), "^D");
        assert_eq!(bar_key("n / p"), "n/p");
        assert_eq!(bar_key("Enter"), "Enter");
    }
}
