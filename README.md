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

`:logs` opens the log browser; `:logs /custom/path/Logs` overrides the server log directory. The sidebar retains tracking choices per connection and applies each choice to the selected log on all hosts. Tracking today carries its mode forward to future days. Historical days can be tracked or untracked individually and always use full on-demand downloads. Earlier days outside the tracked periods are not collected automatically unless explicitly selected. Existing settings without start dates use the earliest known cached update, or the day the settings are migrated if there is no cache. Known tracked files for the selected day remain visible even when empty or unavailable. The three standard ErrorLog names start as on-demand tracking rules; other names start untracked. The compact grid includes all discovered logs, whether tracked or untracked. Ctrl+Left/Right or Ctrl+h/l changes the selected day; `r` enables or disables `0`–`9` day shortcuts and refreshes inventory. Unsuffixed files are shown for today, `_1.txt` for yesterday, `_2.txt` for two days ago, and so on.

The compact sidebar grid has a row for each port and Err, Acc, and Req columns for each day. Green means a fresh cache, yellow means stale, blue means a nonempty remote file still needs downloading, and gray means a tracked file is empty or absent. For previous days, a complete cache matching the last known remote size stays green even when the server is unavailable; a cached copy is also green when a successful host inventory confirms the remote file was deleted. When hosts differ, a mixture of downloaded and missing copies is yellow; blue means no known copy is downloaded. Left and Right prefer the current port and can move to an older or newer rotation. Left/Right or h/l selects another tracked log type; Shift+arrows or H/J/K/L move one cell at a time, including empty and untracked cells. Up/Down selects another tracked port in the current column. Special ports `----` and `TASK` stay at the top, followed by numeric ports in number order. `ErrorLog.txt` uses port `----`; TaskServer logs use `TASK`. A selected cell expands below with host cache size, age, and download status, while the selected log's total size and last update age remain visible. With the file list focused, press `F` or `/` and type part of a filename, host, alias, or port to narrow the grid; `c` clears that filter. Press `p` to open the port selector without changing anything. In the selector, Enter cycles a port through all, include, and exclude; `P` clears all port filters.

The Hosts pane above Files shows each host's alias, name, remote file size and cache age (`n/a` if never downloaded). Files that were not listed or are unavailable are marked accordingly. Use Tab or Shift-Tab to focus Hosts, click a host to include or exclude it, or press `w` in Files to focus the host list: use up/down to select, Space or Enter to toggle, `i` to show only the selected host, `a` to include all hosts, and Esc to return to Files. Use `,` and `.` in Files to scroll the host list when it is taller than the available space. Host choices apply to the displayed and searched log content, including merged and multi-log views; downloads and tracking still operate across hosts. During a download, the status line names the active files and hosts and counts successful and failed checks separately. An unchanged on-demand file counts as a successful check.

Each tracked log has a mode shown by its symbol. For today, press `o` to cycle on-demand, live and partial modes; the chosen mode continues on future days. For a past day, `o` toggles full on-demand tracking for that day only. Press `t` to start or stop tracking the selected day, or `u` to stop tracking it. The choice applies across hosts, and cached content is kept. Press `d` to download the selected file in full without changing tracking:

- **D — on demand:** `s` checks all on-demand files. A file is downloaded in full if its remote byte size differs from its last complete download, or if it has not yet been downloaded fully.
- **L — live:** the app polls the file approximately every five seconds while running. Error logs use timestamp scans. Other logs use an experimental collector which reads the entire remote file when it changes.
- **P — partial:** it is not polled automatically; `d` downloads the complete file.
- **U — untracked:** shown only while discovery is expanded. It is not automatically fetched.

`Enter` opens cached content. Selecting a cell or day column limits displayed and searched entries to that calendar day in server time, even when a file contains older entries. TODAY uses the current server date; selecting a row with Shift+V searches across days. Truncation bounds narrow the selected day further. Each host has one content pane combining its cached rotations for the selected time window; rotations without matching records do not create extra panes. Paging moves the focused pane and aligns the other panes to the nearest timestamp; the aligned lines are highlighted. Use `Tab` and `Shift-Tab` to move focus between Hosts, Files, and the host log panes, or `H` and `L` to switch log hosts directly. Use `d` to download the selected file in full on available hosts without changing tracking. `s` checks on-demand files for changes. `b` also forces a full download and switches an untracked or partial file to on-demand mode. The size-only check can miss a same-size rewrite; use `b` when needed. If the downloaded byte count differs from the reported remote size, the file is checked again on the next requested refresh. A partial cache does not count as a full download.

A failed manual download opens the full host errors automatically; press `Esc` to close them. Press `!` to read full host errors in a scrollable panel. A rejected login (HTTP 401) or disabled AppServer (HTTP 503) pauses automatic collection and queued downloads; cached logs remain readable. Changing a connection's password, authentication method, or TLS settings retires its old log collectors and refreshes the client without deleting cached logs. Enable that AppServer or configure an enabled AppServer in the same environment, then press `R` to retry discovery or `d` to retry the selected download. Temporary service or connection failures back off from 60 seconds to a maximum of five minutes between automatic attempts. File completeness is shown separately from service availability. Full error details and the endpoint are also written to the application diagnostic log, available under F2.

`:logs` opens the viewer before loading local history. A spinner shows metadata loading, archive checks, and initial view preparation, along with the number of cache files checked and elapsed time. Press `q` to close the viewer while loading continues, or `x` to cancel loading.

Manual downloads recheck known remote paths even when saved inventory says a file was empty or absent. Local retained archive references stay out of remote requests. Live catch-up shares a two-request limit across error-log scans and other log reads instead of starting every stream simultaneously.

File navigation moves the selection immediately and updates its content preview after a 300 ms pause. `Enter` or `Tab` opens the selected preview immediately. Content search also waits for a 300 ms typing pause; `Enter` applies it immediately. Idle log screens redraw once per second. File freshness and age summaries are recalculated at most once per minute unless inventory, downloads, or tracking change; live collection retains its five-second polling interval.

A borderless **Log Activity** chart above the log panes shows entry counts across the earliest to latest timestamp in the currently loaded, filtered records, summed across host copies. Its bars are two character cells high: the busiest time bucket fills both cells, quieter buckets scale proportionally with partial block characters, and empty buckets stay blank. White bars sit on alternating muted yellow time bands; the title shows the interval, chosen to suit the time range and width. Multiline messages count once. The histogram runs on a background worker with a spinner while calculating and is cached until the record timestamps or chart width change; scrolling reuses it. For large paged logs, the chart covers the loaded page. Very short terminals hide the chart to preserve space for log text.

Click two positions on the activity chart to select a time window. The selected range is highlighted briefly, then becomes the inclusive truncation window. Click either end first; press `Esc` or another key to cancel the selection. `Shift+U` undoes the change.

To search several logs together, press `Shift+V` in Files to select the current port and log type across all available days, or `Shift+C` to select the current day and log type across all visible ports. Selection respects file and port filters, includes the enabled host copies, and highlights the selected cells in magenta. Press `Enter`, then `/` to search their cached records in chronological order using the current truncation bounds. This also works for RequestLog. Moving to a file or pressing `Esc` clears the selection. Selection itself does not download files; `d` and `b` still operate on the cursor's filename.

A verified full download of a rotated file is settled history when it was downloaded after the file's modification day, using server time. Partial tails and current filenames remain mutable. Settled history can be compressed locally without further server checks, including yesterday's files. The archive is checksum-verified, published durably, and then replaces the raw copy. Legacy complete files without that evidence retain the older 48-hour policy with remote metadata checks. SQLite holds record offsets and timestamps, so either raw or gzip text must remain available for browsing and export.

Completed daily history checks, successful host inventories, and confirmed missing historical files are remembered across restarts. Once that server day's historical inventory is known, automatic reopening requests today's metadata only; the next server day, a new host, or new tracking scope can require discovery again. `R` explicitly rediscovers history and `b` forces a fresh full download. Rotating an archived filename reuses its compressed bytes and any existing SQLite index instead of inflating, copying, and indexing them again. Startup reads settled metadata without eagerly loading historical text, and retained archive day coverage is remembered instead of repeatedly sampled. Archive candidate scanning stops while the catalog is unchanged.

Logs initially shows the full selected calendar day without an additional truncation bound. Explicit truncation bounds exclude older or later cached lines from display, filtering, and search. Visible boundary lines identify entries omitted before an active start bound and after an active end bound. The bounds and shared time cursor persist when changing logs, screens, or connections. Edit or clear bounds with `Ctrl+T`, or use `:reset` to reset the time cursor and clear the truncation bounds.

In a log pane, type the truncation shortcuts quickly, with less than 700 ms between keys. Numbered shortcuts adjust the existing bound; when it is unset, they start from the highlighted line. A single `t` or `T` applies after that short pause, or as soon as you press another unrelated key.

While the application is running, after 1 a.m. server time it refreshes the inventory and completes previous-day files from their tracked periods. It skips a complete local cache when its recorded uncompressed size matches the remote file, including compressed caches. If an unchanged downloaded file has rotated to a new name, the cache is preserved locally under that name without fetching it again. Failed checks retry while the application is running.

Large local files are read in the background; the status line distinguishes building/extending the SQLite index, loading indexed records, scanning as a fallback, and waiting for the local reader. This work reads local disk data, not the server. Indexing progress identifies the host/file and queued count. Appends finish the current pinned read before loading newer bytes; the prepared view stays visible while it updates. Compression preserves prepared views and lets existing raw readers finish before deleting the raw file. Failed reads wait at least 60 seconds before an unchanged query can retry. `x` cancels the read. The viewer limits how much of a large file it holds in memory and warns when entries in the selected window exceed that limit. Narrow the time window to inspect earlier entries. In the all-cached raw view, select a large file individually to see its contents.

`m` merges cached error and access logs for the selected port across all hosts in chronological order. It includes entries for the selected calendar day from cached rotations within the truncation bounds, including cached files that are not currently tracked. The merged title shows the selected date, and moving to another day immediately updates the content. Formatted merged entries show the full server date and time and a host alias such as `h1`; the Files legend maps aliases to hosts. In merged mode the Files grid combines Err and Acc into a Mrg column whose two markers show the Error / Access tracking modes. `\\` hides the sidebar, `v` opens combined display settings, `p` controls ports, and `/` searches cached text. `[` and `]` cycle the number of displayed lines before and after matches. Combined search accepts literal alternatives separated by `|` and `/regex/i`; matching error records retain continuation lines. `s` in the combined pane seeks to a server timestamp, `+`/`-` jump a minute, and `n`/`N` or `e`/`E` jump between matches or errors. `S` saves the displayed view; `V` opens a temporary copy in `$EDITOR`.

| Key | Action |
|---|---|
| `!` | Show full host errors; `R` retries discovery, `Esc` closes |
| `Tab` / `Shift-Tab` | Move focus through the file list and host panes |
| `z` (host pane) | Maximize the focused host pane; press again to restore all panes |
| `#` (log pane) | Toggle displayed line numbers |
| `t` / `T` (log pane) | Set the inclusive start / end of the truncation window at the highlighted line |
| `5t` / `t5` (log pane) | Move the start bound back / forward five minutes; use any number of minutes |
| `5T` / `T5` (log pane) | Move the end bound back / forward five minutes; use any number of minutes |
| `tt` (log pane) | Show five minutes centered on the highlighted line |
| `Ctrl+T` | Edit, adjust, clear, or reset the truncation window |
| `Ctrl+T`, then `t` (or `Ctrl+T` twice) | Clear both truncation bounds quickly |
| Two clicks on Log Activity | Preview and set the truncation window from the selected range |
| `Shift+U` | Undo the most recent truncation-window change |
| `s` (combined pane) | Seek to a server timestamp |
| `j` / `k`, up / down (files) | Move between tracked files in the selected Err, Acc, or Req column |
| `J` / `K`, Shift+up / down (files) | Move one port row, including empty cells |
| `j` / `k`, up / down (log pane) | Move between log messages |
| `Enter` (files) | Open cached content |
| `w` (files) | Choose hosts; Space toggles, `i` selects one, `a` includes all |
| `Shift+V` (files) | Select the current port and log type across all available days |
| `Shift+C` (files) | Select the current day and log type across all visible ports |
| `t` (files) | Start or stop tracking the selected day’s log on every host |
| `o` (files) | Cycle today’s tracking mode, or toggle full on-demand tracking for the selected past day |
| `u` (files) | Untrack the selected name on every host, keeping its cache |
| `s` (files) | Check all on-demand files and download changed files |
| `d` (files) | Download the selected filename in full without changing tracking |
| `Q` | Open the Downloads dialog to see running and queued files, transfer sizes, and failures |
| `Ctrl+D` | Start the file selected in Files immediately, pausing current collection |
| `Enter` / `r` (Downloads) | Start or retry the highlighted download immediately; interrupted downloads resume afterward |
| `x` (Downloads) | Remove the highlighted queued request |
| `b` (files) | Force a full download of the selected filename on available hosts |
| `d` (host pane) | Page down |
| `u` / `PageUp` (host pane) | Page up |
| `H` / `L` (content) | Choose the leading host pane |
| `r` / `R` (files) | Toggle `0`–`9` day shortcuts / refresh inventory |
| Left / Right or h / l (files) | Previous / next tracked log across days |
| Shift+Left / Shift+Right or H / L (files) | Move one cell across days, including empty and untracked cells |
| Ctrl+Left / Ctrl+Right or Ctrl+h / Ctrl+l (files) | Previous / newer discovery day |
| `0`–`9` (files) | Jump to today / 1–9 days ago |
| `F` or `/` (files) | Filter the current file list by name, host, alias, or port |
| `m` / `\\` | Toggle combined view / file sidebar |
| `v` | Combined display options |
| `p` / `i` / `P` (files) | Open port selection / show only included ports / clear port filters |
| `D` | Download both merged logs for the selected port and day; outside merged view, cycle collection history |
| `C` (log pane) | Clear cached content while retaining tracking and the file catalog |
| `f` (host pane) | Toggle following the bottom of the view; live collection is independent |
| `,` / `.` (host pane) | Older / newer indexed page when viewing one large cached file |
| `fi` / `fo` | Edit include / exclude terms, updating after a short pause |
| `fc` | Clear content filters and content search |
| `fs` / `fr` / `fa` | Save a named filter / recall it / add it to the current filter |
| `x` | Cancel the current log operation |
| `/` / `c` | Search content when focused / clear the focused filter |
| `[` / `]` | Cycle the number of displayed lines before / after matches |
| `h` / `l`, `g` / `G`, `PageUp` / `PageDown` | Navigate content or files |
| `Esc` | Close the current dialog or input, or clear the log selection; keep the log view and its bounds |
| `q` | Close the log view; live collection continues while the app runs |
| `?` | Show help |

In a log pane, `fi` and `fo` edit content filters. A term containing uppercase letters is case sensitive; otherwise it is case insensitive. To supply alternatives, start with a delimiter such as `-F, error,timeout`. A record must match at least one include term and no exclude terms. `fa` adds another saved filter as a separate condition: records must pass every layer. Press Tab in the filter editor to edit each layer, Enter to apply, or Esc to undo edits made in that editor. Named filters include the current content search and context settings, are stored with the connection's log cache, and survive clearing cached log content. `fc` clears content filters and search; time windows and file/port selections remain in place. A standalone `f` toggles following after a short pause.

Truncation provides one shared investigation window across log files and hosts during the app session. Focus a log pane and navigate to the highlighted line: use `j`/`k` or the arrow keys to move it; in split host views it is shared across the host panes. Press `t` to keep records at or after that timestamp, and `T` to keep records at or before it. Both bounds are inclusive. A line without a usable timestamp cannot set a bound.

Press `Ctrl+T` to edit the bounds in server-local time. Tab/Shift-Tab switch fields, Up/Down add/subtract the specified number of minutes from the selected bound, Ctrl+U clears a field, Ctrl+R clears both, and Enter applies. Esc closes the dialog and keeps any unfinished edits for the next time it opens. Empty bounds are open-ended. The active window and shared time cursor are shown on screen and survive changing files, pressing Esc, and closing/reopening Logs within the session.

Search, context lines, navigation, merged views, save, and editor export operate only on records inside the window. Access entries without a usable timestamp and records without a preceding timestamp are excluded while truncation is active; continuation lines remain with their message. Truncation changes the local view, not the downloaded cache or server logs. The `D` history setting controls collection; the combined view uses available cached records within the truncation window.

Each time Logs opens, `xdmp:hosts()` discovers the cluster's hosts across all groups, and the app checks tracked filenames on every host. Cached files appear immediately while this happens in the background. The host legend includes discovered hosts even when they have no matching files or their inventory fails; failures appear in the status line. `R` repeats discovery. Untracked filenames remain hidden until `r` is enabled.

Error logs are read with `xdmp:logfile-scan` through `/v1/eval`; other logs use filesystem reads. Cluster hosts are addressed using `file://<host>/...`, so their names need not resolve on the workstation. The account needs eval and the `xdmp-filesystem-directory`, `xdmp-filesystem-file`, and `xdmp-hosts` privileges. Cache files are stored under the platform local-data directory in `marklogic-tui/logs/`, separated by connection and log directory; credentials are not saved there.

The browser indexes large cached files by record offset and time in a background SQLite database and formats visible log rows on demand, retaining a bounded cache around the viewport. The index extends after appended data and is rebuilt for a new generation. Very large lines or records fall back to the background scanner. Single-source large views can page through indexed records; merged and multi-host views remain limited. In a single-source indexed view, `fi`, `fo`, and `/` search the full time window in the background when context lines are off; use `,` and `.` to page through matching records. Formatted relative-time searches, context lines, and export currently operate on the loaded page. Search and export inspect the full selected view. Cached text and indexes remain in memory, full downloads are buffered, and wrapping the combined view measures all records when the layout changes. The planned disk-backed reader and streaming transfers are described in [the log interface plan](docs/log-interface-plan.md).

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

The System **Storage** tab is a grid of cached log data, with one column per calendar
day and one row per port, across all local environments and hosts. System logs have a
**System** row and task-server logs a **TASK** row. Dates use the log's timestamps;
explicit time zones are converted to UTC. Records without a date appear under
**Unknown**, and continuations stay with their preceding record.

Each cell, port total, date total, and the overall total shows **T** (thawed, green)
and **F** (frozen, cyan) sizes separately. Frozen sizes are the compressed payload on
disk; thawed sizes are the retained uncompressed bytes. These totals exclude record
indexes, metadata, and older unreferenced cache files. A `~` marks an estimated daily
share of an older whole-file archive; managing it creates exact daily partitions.

| Storage control | Action |
| --- | --- |
| Arrow keys / `h/j/k/l` | Move between dates and ports; the grid scrolls as needed |
| `Space` / Enter | Select the focused cell, row, or column |
| `s` | Cycle selection scope: cell, row, column |
| `R` / `C` | Select the entire focused port row / date column |
| `v` | Toggle multi-select; selecting an already selected group removes it |
| `c` / Esc | Clear selection |
| `*` | Select all populated cells |
| Click cell / port / date header | Select that cell / row / column |
| Ctrl-click or Shift-click | Add or remove a cell or group |
| `a` / `t` / `d` | Review archive / thaw / delete for the current selection |
| Enter / `y` in review | Apply the action |
| Esc / `n` in review | Cancel the action |
| `r` | Refresh the inventory |

Actions run in the background and affect only the selected days and ports. A cell may
contain several hosts or source files; the selection summary shows its size and source
count before confirmation. Daily partitions preserve original record ordering, and
Logs, Reports, search, and export read both frozen and thawed data. Live appends remain
thawed while older frozen days stay compressed. Deletion preserves remote collection
cursors; a deliberate full re-download can fetch deleted local records again.

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
