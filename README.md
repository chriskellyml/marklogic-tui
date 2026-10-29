# marklogic-tui

A terminal user interface (TUI) client for [MarkLogic](https://www.marklogic.com/) databases, written in Rust.

Browse documents, run JavaScript queries, manage collections, and interact with MarkLogic — all from the terminal.

## Features

- Browse and paginate documents across databases
- Filter documents by collection or URI pattern
- Edit and run query files from the launch directory
- Track favorite and recent query folders
- Execute JavaScript and XQuery files via MarkLogic's `/v1/eval` endpoint
- View documents full-screen with pretty-printed JSON/XML and metadata
- Delete documents (multi-select + confirm)
- Browse collections and drill into them
- List and switch between databases
- Manage multiple named server profiles
- Browse MarkLogic logs by host, cache tails or full files, search locally, and track transfer costs
- Run editable YAML reports, starting with per-host memory history from downloaded error logs
- Inspect TUI activity and configure local preferences in System mode

## Prerequisites

- [Rust](https://rustup.rs) 1.85+ (edition 2024)
- A running MarkLogic instance accessible over HTTP/HTTPS

## Build and Run

```bash
git clone <repo-url>
cd marklogic-tui

# Run directly (debug build)
cargo run

# Or build a release binary
cargo build --release
./target/release/marklogic-tui
```

## First-Time Setup

1. Launch the app: `cargo run`
2. Type `:servers` and press `Enter` (or `:server-add`)
3. Press `a` to open the Add Server wizard
4. Fill in the fields (Tab to advance between fields):
   - **Name** — a friendly label (e.g. `local`)
   - **URI** — base URL, no trailing slash (e.g. `http://localhost`)
   - **Username** / **Password** — MarkLogic credentials
   - **Port** — App server port (default `8000`)
5. Press `Enter` to save and switch to the new server
6. Type `:databases`, select your database, press `Enter`
7. Documents load automatically — use the commands below to explore

## Configuration

Config is stored in the platform config directory:

- **macOS**: `~/Library/Application Support/marklogic-tui/config.toml`
- **Linux**: `~/.config/marklogic-tui/config.toml`

The file is created and updated automatically by the app. You can also edit it manually:

```toml
active_server = "local"
active_database = "Documents"

[[servers]]
name = "local"
uri = "http://localhost"
username = "admin"
password = "admin"
port = 8000
```

> Note: Passwords are stored in plaintext in the config file.

Tracked query folders are stored separately in `~/.marklogic-tui/folders.toml`.
Per-folder cached query results remain in each query folder's own `.marklogic-tui/` directory.

Use `--config FILE` to import a YAML config bundle before starting. While that bundle is active,
server, app-server, favorite, active-selection, variable, and tracked-folder changes are written
back to `FILE` as well as to the normal config locations. Use `--export-config FILE` to export the
current state as a YAML bundle without starting the app.

## Commands

Type `:` followed by a command and press `Enter`. Tab-completion is available.

| Command | Description |
|---|---|
| `:servers` | Open server management popup |
| `:server-add` | Open the add server wizard directly |
| `:databases` | List databases and open selection popup |
| `:list` | List all documents (clears any active filters) |
| `:collections` | Open collection browser |
| `:list:<collection>` | List documents in a specific collection |
| `:clear` | Clear collection/URI filters and reset pagination |
| `:tdes` | List TDE (Template Driven Extraction) templates |
| `:query` | Show the active query file editor |
| `:query-files` | Open the query file picker |
| `:query-open` | Open the query file picker |
| `:folders` | Open the tracked folder selector |
| `:logs [directory]` | Open the MarkLogic log browser (optional server log directory) |
| `:reports [directory]` | Run reports from downloaded logs (optional server log directory, matching `:logs`) |
| `:system` | Open TUI diagnostics and system preferences |
| `:app-logs` | Open the Logs tab in System (also F2) |
| `:quit` | Quit the application |

## Keyboard Shortcuts

### Global

| Key | Action |
|---|---|
| `Ctrl+C` | Quit |
| `Ctrl+1` / `Ctrl+2` / `Ctrl+3` | Switch to Query / Logs / Reports |
| `Ctrl+4` / `Ctrl+5` | Switch to Servers / Documents |
| `Ctrl+6` | Switch to System |
| `Ctrl+.` | Toggle between the current and previous screens |
| `Alt+1`–`Alt+6` / `Alt+.` | Equivalent screen shortcuts for terminals that do not report the Ctrl combinations |
| `Esc` | Reset the current screen, leave insert mode, or dismiss the current dialog |
| `Esc` `Esc` (double, within 500ms) | Return to the centered start page |
| `:` | Focus the command input |

Click the environment name in the top-left corner to choose a server. Click a pane or its title to focus it; in the log browser, clicking a host pane also makes it the time reference. Mouse capture is enabled while the TUI is active.

Screen shortcuts preserve each screen's state, including query edits, log position, and open forms. Ctrl+5 opens the current document list; use `:list` to load or refresh it. The previous screen also updates when navigating with commands or closing a screen. The app requests enhanced keyboard reporting from terminals that support it; some terminals or terminal multiplexers cannot distinguish Ctrl-number or Ctrl-period, or reserve them for their own shortcuts. Use the Alt equivalents there.

### Results List

| Key | Action |
|---|---|
| `j` / `Down` | Move selection down |
| `k` / `Up` | Move selection up |
| `Enter` | Open selected document (full-screen view) |
| `Space` | Toggle selection (for bulk delete) |
| `n` | Next page |
| `p` | Previous page |
| `/` | Open URI filter input |
| `Ctrl+D` | Delete selected documents (opens confirmation) |

### Command Input

| Key | Action |
|---|---|
| `Enter` | Execute command / accept autocomplete |
| `Tab` | Accept highlighted autocomplete suggestion |
| `Up` / `BackTab` | Cycle autocomplete up |
| `Down` | Cycle autocomplete down |
| `Esc` | Clear autocomplete suggestions |
| `Backspace` | Delete last character |
| `/` (when empty) | Switch to URI filter mode |

### URI Filter Input

| Key | Action |
|---|---|
| `Enter` | Apply filter and re-fetch |
| `Esc` | Cancel, return to results |
| `Backspace` | Delete last character |

### Query Editor

| Key | Action |
|---|---|
| `Alt+Enter` / `Ctrl+Enter` / `F5` | Execute the query |
| `c` | Open module clone picker |
| `e` | Open the current query-pane contents in `$EDITOR`, then load the saved edits back |
| `Ctrl+E` | Open `$EDITOR` from either inline editing mode (e.g. Neovim when `EDITOR=nvim`) |
| `Ctrl+S` | Save the active query file immediately |
| `Ctrl+O` | Open the query file picker |
| `f` (when Files list is focused) | Open the tracked folder selector |
| `Esc` | Leave insert mode or reset the current view |

Supported launch-directory file types: `.js`, `.mjs`, `.sjs`, `.xqy`, `.sql`, `.sparql`.

- Phase 1 execution support: `.js`, `.mjs`, `.sjs`, `.xqy`
- Phase 1 editing/switching only: `.sql`, `.sparql`
- Editor changes autosave after you stop typing (2 seconds by default; configurable in System)

### Editor and Syntax Highlighting

The query editor is [edtui](https://github.com/preiter93/edtui) with Vim-style modes. It starts in Normal mode.

- `i`, `a`, `o` or `v` enter Insert or Visual mode. `Esc` returns to Normal mode.
- In Normal mode the Files pane and app commands take their keys: `j`/`k`, `g`/`G`, `Enter`, `n`, `f`, `m`, `e`, `r`, `R`, `X`, `Delete` and `\`. Other keys such as `x` still reach the editor. Hide the Files pane with `\` to get every Vim key in Normal mode.
- In Insert mode every key is typed, including `1`, `\`, `?` and `:`. The run shortcuts still work.
- `--inline-editor textarea` starts the simpler editor, which has no syntax highlighting.

Highlighted: `.xqy` (XQuery grammar in `assets/xquery.sublime-syntax`), `.js`, `.mjs` and `.sjs` (JavaScript), and `.sql`. `.sparql` has no grammar and is shown plain.

Limitation: edtui highlights each line on its own, so a `(: :)` comment or string that spans several lines is coloured only on its first line.

### Files Tree

The Files pane shows query files and folders under the launch directory as a tree. Folders start expanded. Hidden entries (names starting with `.`, including `.marklogic-tui`) are not shown.

| Key | Action |
|---|---|
| `j` / `k` / `Up` / `Down` | Move selection (opens files as you land on them) |
| `g` / `G` | First / last row |
| `Enter` | Open a file, or expand/collapse a folder |
| `M` (Shift+m) | Rename or move the selected file or folder |
| `n` | New query file (accepts `sub/dir/name.xqy`) |
| `Delete` | Delete the selected file (confirm), or an empty folder |

- Rename and new-file names may contain folders, such as `foo/foo/foo/blah.xqy`. Missing folders are created.
- Query files must be inside a folder. Files directly in the launch folder are not shown; the app reports how many there are. The default location for them is `~/.marklogic-tui/queries/`.
- Renamed files must keep a supported extension. Folders can be renamed or moved, including their contents and cached results.
- Folders are removed automatically once they are empty. To keep a folder, put a file in it.
- The Files and Modules panes share one width: the widest visible entry in either tree, plus 2 columns for the selection marker, plus the borders. It is clamped to 15 to 100 columns.
- Folder names are dimmed so file names stand out.
- A query that finishes with no output shows `No output` in yellow in the results pane.
- Renaming an item is refused while a query under it is running.

### Modules Cache

Checked-out modules have separate working copies and manifests for each connected
environment, even when those environments use the same modules database name. Switching
servers therefore shows only that server's checkouts and sync state. In the Modules pane,
`r` refreshes the selected module from the active server and replaces both its original
snapshot and working copy; it never pushes an older cached version back to the server.

### XQuery Imports

`.xqy` queries can pull declarations from other files in the query folder. Before a query runs, each directive is replaced in place by the matching `declare` from the referenced file:

```xquery
import variable lib/clean-process.xqy:GLOB_VAR;
import function clean-process.xqy:my-function#4;

local:my-function(1, 2, 3, 4)
```

- Paths are relative to the directory of the file containing the directive.
- Functions are matched by name and arity (`#N`); variables by name.
- Declarations keep their original prefix, so call them as declared in the source file (e.g. `local:my-function`).
- Dependencies are not pulled in automatically: if an imported function uses `$GLOB_VAR`, add its `import variable` line too.
- Imported files may contain their own imports. Circular imports and conflicting declaration names are reported as errors.
- The editor keeps the directives; only the text sent to MarkLogic is expanded. Line numbers are preserved.

### Tracked Folder Selector

| Key | Action |
|---|---|
| `Enter` | Switch to selected folder |
| `a` | Add a tracked folder by path |
| `f` | Toggle favorite |
| `d` | Remove tracked folder entry |
| `c` | Clear that folder's cached query results |
| `Esc` | Close popup |

### Full-Screen Document View

| Key | Action |
|---|---|
| `Esc` | Reset scroll to the top |
| `q` | Close and return to results |
| `e` | Edit the current document in `$EDITOR` and save it back to MarkLogic |
| `?` | Show/hide keybindings help |
| `j` / `Down` | Scroll down one line |
| `k` / `Up` | Scroll up one line |
| `Space` / `PageDown` | Scroll down 20 lines |
| `PageUp` | Scroll up 20 lines |

### MarkLogic Log Browser

`:logs` opens the log browser; `:logs /custom/path/Logs` overrides the server log directory. The sidebar retains tracking choices per connection and groups the same filename across hosts. Known tracked files for the selected day remain visible even when empty or unavailable. The three standard ErrorLog names start as on-demand tracking rules; other names start untracked. The compact grid includes all discovered logs, whether tracked or untracked. Ctrl+Left/Right or Ctrl+h/l changes the selected day; `r` enables or disables `0`–`9` day shortcuts and refreshes inventory. Unsuffixed files are shown for today, `_1.txt` for yesterday, `_2.txt` for two days ago, and so on.

The compact sidebar grid has a row for each port and Err, Acc, and Req columns. Left/Right or h/l selects another tracked log type; Shift+Left/Right or H/L also selects untracked types. Up/Down selects another port and highlights that row. Special ports `----` and `TASK` stay at the top, followed by numeric ports in number order. `ErrorLog.txt` uses port `----`; TaskServer logs use `TASK`. A selected cell expands below with host cache size, age, and download status, while the selected log's total size and last update age remain visible. With the file list focused, press `F` or `/` and type part of a filename, host, alias, or port to narrow the grid; `c` clears that filter. Press `p` to open the port selector without changing anything. In the selector, Enter cycles a port through all, include, and exclude; `P` clears all port filters.

In the Files list, the selected filename expands to show each host's remote file size and the age of its local cache (`n/a` if never downloaded). The compact rows use `h1`, `h2`, and so on; the legend immediately below Files maps those aliases to full hostnames. Files that were not listed or are unavailable are marked accordingly. If the host rows are longer than the available space, use `,` and `.` with the file list focused to scroll the rows and legend together. During a download, the status line names the active files and hosts and counts successful and failed checks separately. The host rows mark the outcome for the selected file. An unchanged on-demand file counts as a successful check.

Each file has a mode, shown by a letter and color. Press `o` to cycle the selected filename through on-demand, live and partial modes on every host. Press `u` in the file list to untrack it explicitly; its cached content is kept:

- **D — on demand:** `s` checks all on-demand files, and `d` checks the selected one. A file is downloaded in full if its remote byte size differs from its last complete download, or if it has not yet been downloaded fully.
- **L — live:** the app polls the file approximately every five seconds while running. Error logs use timestamp scans. Other logs use an experimental collector which reads the entire remote file when it changes.
- **P — partial:** `d` fetches a bounded preview; it is not polled automatically.
- **U — untracked:** shown only while discovery is expanded. It is not automatically fetched.

`Enter` opens cached content. Each non-empty downloaded host copy appears in its own pane. Paging moves the focused pane and aligns the other panes to the nearest timestamp; the aligned lines are highlighted. Use `Tab` and `Shift-Tab` to move focus between the file list and host panes, or `H` and `L` to switch hosts directly. On-demand files are downloaded when requested with `d` or `s`. `b` forces a full download of the selected group on available hosts, even if the size is unchanged, and switches an untracked or partial file to on-demand mode. The size-only check can miss a same-size rewrite; use `b` when needed. If the downloaded byte count differs from the reported remote size, the file is checked again on the next requested refresh. A partial cache does not count as a full download.

The first time Logs is opened in an app session, its default start bound is set to ten minutes before the current server time. Older cached lines are excluded from display, filtering, and search; the window then grows forward as live log records arrive. Visible boundary lines identify entries omitted before an active start bound and after an active end bound. The bounds and shared time cursor persist when changing logs, screens, or connections. Edit or clear bounds with `Ctrl+T`, or use `:reset` to reset the time cursor and restore the default start bound.

`m` merges cached error and access logs for the selected port across all hosts in chronological order. It includes cached rotations within the truncation bounds, including cached files that are not currently tracked. Formatted merged entries show the full server date and time and a host alias such as `h1`; the Files legend maps aliases to hosts. In merged mode the Files grid combines Err and Acc into a Mrg column whose two markers show the Error / Access tracking modes. `\\` hides the sidebar, `v` opens combined display settings, `p` controls ports, and `/` searches cached text. `[` and `]` cycle the number of displayed lines before and after matches. Combined search accepts literal alternatives separated by `|` and `/regex/i`; matching error records retain continuation lines. `s` in the combined pane seeks to a server timestamp, `+`/`-` jump a minute, and `n`/`N` or `e`/`E` jump between matches or errors. `S` saves the displayed view; `V` opens a temporary copy in `$EDITOR`.

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | Move focus through the file list and host panes |
| `z` (host pane) | Maximize the focused host pane; press again to restore all panes |
| `#` (log pane) | Toggle displayed line numbers |
| `t` / `T` (log pane) | Set the inclusive start / end of the truncation window at the highlighted line |
| `Ctrl+T` | Edit, adjust, clear, or reset the truncation window |
| `Shift+U` | Undo the most recent truncation-window change |
| `s` (combined pane) | Seek to a server timestamp |
| `j` / `k`, up / down (files) | Move between tracked files in the selected Err, Acc, or Req column |
| `J` / `K`, Shift+up / down (files) | Move one port row at a time |
| `j` / `k`, up / down (log pane) | Move between log messages |
| `Enter` (files) | Open cached content |
| `o` (files) | Cycle on-demand, live, and partial for the selected name on every host; start tracking an untracked file |
| `u` (files) | Untrack the selected name on every host, keeping its cache |
| `s` (files) | Check all on-demand files and download changed files |
| `d` (files) | Refresh the selected filename; in merged view download both Error and Access logs for the selected port and day |
| `b` (files) | Force a full download of the selected filename on available hosts |
| `d` (host pane) | Page down |
| `u` / `PageUp` (host pane) | Page up |
| `H` / `L` (content) | Choose the leading host pane |
| `r` / `R` (files) | Toggle `0`–`9` day shortcuts / refresh inventory |
| Left / Right or h / l (files) | Previous / next tracked log column |
| Shift+Left / Shift+Right or H / L (files) | Previous / next log column, including untracked files |
| Ctrl+Left / Ctrl+Right or Ctrl+h / Ctrl+l (files) | Previous / newer discovery day |
| `0`–`9` (files) | Jump to today / 1–9 days ago |
| `F` or `/` (files) | Filter the current file list by name, host, alias, or port |
| `m` / `\\` | Toggle combined view / file sidebar |
| `v` | Combined display options |
| `p` / `i` / `P` (files) | Open port selection / show only included ports / clear port filters |
| `D` | Download both merged logs for the selected port and day; outside merged view, cycle collection history |
| `C` | Clear cached content while retaining tracking and the file catalog |
| `f` (host pane) | Toggle following the bottom of the view; live collection is independent |
| `x` | Cancel the current log operation |
| `/` / `c` | Search content when focused / clear the focused filter |
| `[` / `]` | Cycle the number of displayed lines before / after matches |
| `h` / `l`, `g` / `G`, `PageUp` / `PageDown` | Navigate content or files |
| `Esc` | Close the current dialog or input; keep the log view and its bounds |
| `q` | Close the log view; live collection continues while the app runs |
| `?` | Show help |

Truncation provides one shared investigation window across log files and hosts during the app session. Focus a log pane and navigate to the highlighted line: use `j`/`k` or the arrow keys to move it; in split host views it is shared across the host panes. Press `t` to keep records at or after that timestamp, and `T` to keep records at or before it. Both bounds are inclusive. A line without a usable timestamp cannot set a bound.

Press `Ctrl+T` to edit the bounds in server-local time. Tab/Shift-Tab switch fields, Up/Down add/subtract the specified number of minutes from the selected bound, Ctrl+U clears a field, Ctrl+R clears both, and Enter applies. Esc closes the dialog and keeps any unfinished edits for the next time it opens. Empty bounds are open-ended. The active window and shared time cursor are shown on screen and survive changing files, pressing Esc, and closing/reopening Logs within the session.

Search, context lines, navigation, merged views, save, and editor export operate only on records inside the window. Access entries without a usable timestamp and records without a preceding timestamp are excluded while truncation is active; continuation lines remain with their message. Truncation changes the local view, not the downloaded cache or server logs. The `D` history setting controls collection; the combined view uses available cached records within the truncation window.

Each time Logs opens, `xdmp:hosts()` discovers the cluster's hosts across all groups, and the app checks tracked filenames on every host. Cached files appear immediately while this happens in the background. The host legend includes discovered hosts even when they have no matching files or their inventory fails; failures appear in the status line. `R` repeats discovery. Untracked filenames remain hidden until `r` is enabled.

Error logs are read with `xdmp:logfile-scan` through `/v1/eval`; other logs use filesystem reads. Cluster hosts are addressed using `file://<host>/...`, so their names need not resolve on the workstation. The account needs eval and the `xdmp-filesystem-directory`, `xdmp-filesystem-file`, and `xdmp-hosts` privileges. Cache files are stored under the platform local-data directory in `marklogic-tui/logs/`, separated by connection and log directory; credentials are not saved there.

The browser indexes cached records and formats visible log rows on demand, retaining a bounded cache around the viewport. Search and export inspect the full selected view. Cached text and indexes remain in memory, full downloads are buffered, and wrapping the combined view measures all records when the layout changes. The planned disk-backed reader and streaming transfers are described in [the log interface plan](docs/log-interface-plan.md).

### System

Open `:system` or press Ctrl+6 / Alt+6 to inspect the TUI itself. System works without a
MarkLogic connection. Tab/Shift+Tab switch between **Logs** and **Settings**; you can also
click the tabs. `q` returns to the screen you came from, `:` opens commands, and `?` shows help.

The Logs tab shows the application's diagnostic file, `/tmp/marklogic-tui.log`, including
screen changes, query activity, background operations, and errors. F2 and `:app-logs` open
this tab directly. New entries appear automatically. `f` toggles following the latest entry;
scrolling with `j/k`, arrows, PageUp/PageDown, or `d/u` stops following. `gg` goes to the top,
`G` resumes following at the bottom, and `h/l` scroll horizontally. `/` edits a case-insensitive
text filter (Enter/Esc finish; Ctrl+U clears the input), `c` clears the filter, `r` refreshes,
and `e` opens the complete diagnostic file in `$EDITOR`.

The viewer reads at most the last 1 MiB and displays at most 10,000 recent lines; the complete
file remains on disk. Unchanged files are not reread on each redraw. Diagnostic logs are
rotated at startup and older rotated logs are compressed.

In Settings, use `j/k` to select a preference and Enter/Space or Left/Right to change it:

- **Diagnostic log level:** Error, Warn, Info, Debug (default), or Trace. Changes which new
  entries are written immediately; existing entries remain visible.
- **Inline query editor:** edtui (default, Vim modes and syntax highlighting) or textarea.
  Switching preserves the current query text and returns the editor to navigation mode.
  `--inline-editor` overrides the saved preference at startup.
- **Query autosave delay:** 1, 2 (default), 5, 10, or 30 seconds after the last edit.
  Ctrl+S still saves immediately.

Preferences apply immediately and save to the normal configuration file, plus the active
`--config` YAML bundle when present. System also shows the version, process ID, uptime,
running query count, and local configuration/query/cache paths. Saved preferences use:

```toml
[system]
log_level = "debug"
inline_editor = "edtui"
autosave_seconds = 2
```

### Reports

Open `:reports` to browse report definitions in the left pane. Press `e` to edit the selected YAML file **in place** with `$EDITOR`, `r` (or F5) to run it, and `R` to reload the list. Invalid definitions stay visible so they can be repaired with `e`. Every run rereads and validates the YAML. Tab switches between the list and sample output; `j/k`, `g/G`, and PageUp/PageDown scroll. `?` opens help; `q` returns to the landing page.

The first report, **Memory history**, reads downloaded `ErrorLog.txt` and `ErrorLog_*.txt` records for the active server/account. Download or refresh those logs in `:logs` first. The report does not contact the server. If you use a custom server log directory, pass the same directory to both `:logs` and `:reports`.

The run form accepts a day (`YYYY-MM-DD`, or `latest` for the newest day with memory records) and an optional exact host name (blank selects all hosts). Tab/Shift+Tab move between fields, Ctrl+U clears a field, Enter runs, and Esc cancels. Days and chart times use the timestamps as written by the server, without timezone conversion. With a host filter, `latest` selects that host's latest day.

The chart shows each host's observed memory percentages across 00:00–24:00, with a timestamped sample table below. Press `m` to cycle metrics: `used` is the leading `Memory N%` value; `rss`, `size`, and other available percentages come directly from the corresponding log fields. Missing metrics remain absent. Points are not interpolated across gaps; overlapping cached records are deduplicated. Cache failures and malformed memory records are reported, and cached history may cover only part of the day. Only committed cache content is read; uncommitted download tails are excluded.

Report YAML lives in a `reports/` directory alongside `config.toml` (on macOS, `~/Library/Application Support/marklogic-tui/reports/`; on Linux, `~/.config/marklogic-tui/reports/`). The built-in definition is copied there on first use and subsequent edits are preserved. Add more `.yaml` or `.yml` files to this directory and press `R` to discover them. The source example is [reports/memory-history.yaml](reports/memory-history.yaml).

Version 1 has the following schema:

```yaml
version: 1
name: Memory history
description: Memory percentages per host through a day.
inputs:
  - id: day
    label: Day (YYYY-MM-DD, or latest)
    default: latest
  - id: host
    label: Host (blank for all hosts)
    default: ''
steps:
  - type: cached_error_logs
  - type: memory_history
    day_input: day
    host_input: host
```

Inputs are text fields with `id`, `label`, optional `default`, and optional `required` (false by default). Step input references must name declared input IDs. Version 1 supports `cached_error_logs` followed by `memory_history`; unsupported versions, steps, fields, and step order fail validation. Server queries, log downloads/refresh, meters, and local JavaScript/shell execution are future step types, to be added as the report DSL grows.

### Servers Interface

| Key | Action |
|---|---|
| `Tab` | Switch between Favorites and Servers |
| `j` / `Down` | Move selection down |
| `k` / `Up` | Move selection up |
| `g` / `G` | Jump to top/bottom |
| `Enter` | Fold/open a server, or activate an app server. Expanded servers show Group -> App Server entries; legacy app servers use the `Default` group |
| `s` | Activate selected server |
| `Ctrl+F` | Add/remove the selected Server -> App Server favorite |
| `a` | Add a server or app server |
| `e` | Edit the selected server/app server |
| `d` | Delete the selected server/app server |
| `r` | Connect to the selected server, list remote app servers, and copy the checked group/app-server entries into the local config. Unchecked local entries are preserved |
| `Esc` | Reset the current selection |
| `q` | Close the interface |

### Server Add Wizard

| Key | Action |
|---|---|
| `Tab` | Next field |
| `BackTab` | Previous field |
| `Enter` | Save server |
| `Esc` | Cancel |

### Database / Collection Select Popups

| Key | Action |
|---|---|
| `Enter` | Select highlighted item |
| `j` / `Down` | Move down |
| `k` / `Up` | Move up |
| `Esc` | Close popup |

### Delete Confirmation

| Key | Action |
|---|---|
| `y` | Confirm deletion |
| `n` / `Esc` | Cancel |

## Architecture

The app is a single-binary Rust application using:

- **[ratatui](https://github.com/ratatui-org/ratatui)** — TUI rendering
- **[crossterm](https://github.com/crossterm-rs/crossterm)** — terminal input/output
- **[tokio](https://tokio.rs)** — async runtime
- **[reqwest](https://github.com/seanmonstar/reqwest)** — HTTP client
- **[tui-textarea](https://github.com/rhysd/tui-textarea)** — multi-line editor widget
- **[digest_auth](https://crates.io/crates/digest-auth)** — HTTP Digest Authentication

MarkLogic REST APIs used: `/v1/search`, `/v1/eval`, `/v1/documents`, `/v1/rows`, and the Manage API on port 8002.

### Report checks

```bash
cargo test reports
```

### Log collector checks

Run the deterministic tests without the older client tests that require a
`data-platform-content` database:

```bash
cargo test -- --skip client::tests::test_
```

A separate opt-in smoke test reads host inventories and bounded ErrorLog tails
from a local cluster, then verifies that a repeat fetch resumes without resetting:

```bash
MARKLOGIC_LOG_TEST_CONFIG="/absolute/path/to/config.toml" \
  cargo test live_cluster_log_extraction -- --ignored --nocapture
```

Unit-test settings and tracked-folder writes use isolated temporary directories.
