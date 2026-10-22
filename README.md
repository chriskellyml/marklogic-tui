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
| `:app-logs` | Open the TUI diagnostic log (also F2) |
| `:quit` | Quit the application |

## Keyboard Shortcuts

### Global

| Key | Action |
|---|---|
| `Ctrl+C` | Quit |
| `Esc` `Esc` (double, within 500ms) | Return to the centered start page |
| `:` | Focus the command input |

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
| `Esc` | Return focus to command input |

Supported launch-directory file types: `.js`, `.mjs`, `.sjs`, `.xqy`, `.sql`, `.sparql`.

- Phase 1 execution support: `.js`, `.mjs`, `.sjs`, `.xqy`
- Phase 1 editing/switching only: `.sql`, `.sparql`
- Editor changes autosave every 2 seconds after you stop typing

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
| `Esc` / `q` | Close and return to results |
| `e` | Edit the current document in `$EDITOR` and save it back to MarkLogic |
| `?` | Show/hide keybindings help |
| `j` / `Down` | Scroll down one line |
| `k` / `Up` | Scroll up one line |
| `Space` / `PageDown` | Scroll down 20 lines |
| `PageUp` | Scroll up 20 lines |

### MarkLogic Log Browser

`:logs` opens a separate full-screen area with a host/file list and cached content.
`:logs /custom/path/Logs` overrides the directory; otherwise the connected server's
data directory plus `/Logs` is used on each host.

Logs are **tracked incrementally**. By default only `8000_ErrorLog.txt`, `ErrorLog.txt`
and `TaskServer_ErrorLog.txt` are tracked, on every host, and only for the current day
(server time). Rotated archives (`*_1.txt`, `*_2.txt`, …) stay hidden from the sidebar
by default, including archives created today; use the file filter (`F`) to surface one
when needed. Every other current-window log (other ports,
access, request, audit, numbered archives) is fetched only when you press `o` to track
it, or `Enter` for a one-off fetch. Tracking choices are remembered per connection;
the history window is session-only and resets to today whenever you enter the log view.

The file sidebar puts tracked streams first, then sorts the remaining streams by host,
port, and log kind. It uses short host aliases and compact names such as `h1. E`,
`h1. 9060_E`, and `h1. 9060_A`; each row also shows its last-known size. A tracked
ErrorLog's size is refreshed on every poll, while untracked rows retain the size from
their most recent inventory refresh. Its bottom legend maps each alias to the full host
name. With the sidebar focused, `F` discovers current-day logs and opens a
case-insensitive filter; `c` clears it. `/` remains content search.

Opening the view shows what is cached, then catches up only already-known tracked logs
in the background—it does not scan every host's log directory. Press `r` to explicitly
discover logs on the cluster, or press `F` to discover and filter in one step. After
finding a row, press `o` to add that log stream to tracking. Error-log collection is
bounded by the selected history
window (today by default): it never resumes an older cursor by silently backfilling days
of history. Changing the window starts a fresh generation for that window, and narrowing
it also trims locally cached ErrorLog content. While the view is open and following,
tracked logs are polled every 5 seconds; a poll with nothing new moves about 1.5 KB.

Press `m` for a native combined view inspired by `tmp/follow-ml-logs.sh` and
`pp-pretty-oneline.js`. It merges **tracked** streams chronologically across hosts,
groups `Info+` / `Error+` continuations with their parent records, and shows compact
colored `ERR` / `REQ` / `ACC`, port and timestamp columns. Access logs show client,
method, status, size and decoded URL; request logs group time and cache metrics.
XML errors show decoded summaries and stack locations. Original cached text stays
available in raw mode. Explicit timestamp offsets are converted to server time;
date separators keep midnight transitions clear. Other logs are not auto-tracked.

`\\` hides the sidebar so the stream uses the full width. `v` opens display settings
for raw output, relative time, compact/full XML errors, wrapping, extra details,
search modes (off/highlight/filter), and log types. `p` in combined content opens
port selection without requiring the sidebar; `i` restricts the view to marked
include ports and `P` clears the marks. `a` expands the combined view to all cached
sources, still respecting the current history window, port/type and search filters.
`t` seeks to `HH:MM:SS` or `YYYY-MM-DD HH:MM:SS` in server time; `+`/`-` jump a minute.
`n`/`N` jump between matching records, `e`/`E` between errors. `S` saves the displayed
view under the log cache; `V` opens it in `$EDITOR`. Combined search accepts literals
or `/regex/i`, separated by `|` (alternation inside a regex stays intact), and keeps
all lines of a matching record together. Scroll up to review history while collection
continues; `G` or enabling `f` returns to the tail.

| Key | Action |
|---|---|
| `Tab` | Switch between file list and content |
| `j` / `k`, arrows | Select files or scroll content |
| `o` | Track / untrack the selected log (applies to that name on every host) |
| `Enter` / `d` (files) | Fetch new records for the selected file, tracked or not |
| `d` / `u` (content) | Page down / up |
| `m` / `\\` | Toggle combined view / file sidebar |
| `v` / `p` (combined content) | Display options / port selection |
| `s` | Catch up all tracked logs now |
| `D` | Cycle history window: today, 2, 7, 30 days (wider windows refetch and download more) |
| `C` | Clear cached log content, reset the window to today, and sync again |
| `b` | Download an entire file in one query (reads the whole file on the server) |
| `r` | Refresh host and file inventory |
| `p` / `i` / `P` | Cycle port include/exclude marks, show only `+` ports, clear marks |
| `f` | Toggle following (5 s polling; the view sticks to the bottom only if it was there) |
| `x` | Cancel the current request and pause following |
| `F` (files) | Discover current-day logs and filter the log-file list |
| `/` | Edit local content search; Enter applies, Esc cancels and clears |
| `a` | Toggle viewing/searching all cached logs, grouped by host/file |
| `c` | Clear the focused file-list filter or content search |
| `h` / `l` | Scroll horizontally |
| `g` / `G` | First/last file, or top/bottom of content |
| `PageUp` / `PageDown` | Scroll content |
| `Esc` / `q` | Close and pause collection |
| `?` | Show help |

Error logs are read with `xdmp:logfile-scan`, which filters by timestamp on the server,
through the same `/v1/eval` endpoint as the rest of the app (no management port
needed, so tunnelled profiles work). A stream's rotated archives (`ErrorLog_1.txt`, ...)
are included only when they can hold records in the window, so MarkLogic's midnight
rotation loses nothing. Every host is addressed through `file://<host>/...` paths, so
cluster node names need not resolve on your workstation. The account needs eval
privileges plus `xdmp-filesystem-directory`, `xdmp-filesystem-file` and `xdmp-hosts`.
Missing hosts, files and permissions are reported while cached content stays available.

Non-error logs (access, request, audit) have no timestamp-addressed read, so tracked
ones fall back to character cursors: a changed file is read **in full on the server**
and only the new characters are returned, in chunks of at most 65,536 characters.
Track those only when you need them. `b` downloads a whole file once.

The view reports remote file bytes, cached log bytes, session HTTP response body
bytes (including JSON/multipart, error responses and digest challenges), and
last-operation/per-host totals. HTTP headers, TLS overhead, and uploads are excluded.
The server read estimate applies only to the character-cursor fallback.

Cache files live in the platform local-data directory under `marklogic-tui/logs/`,
separated by server URL, profile, username and log directory. The cache path is shown
in the view. Each stream is a `.log` file (usable with tools such as
`rg -i --glob '*.log' pattern <cache-path>`) plus a small JSON cursor; new records are
appended while a window change can compact ErrorLog content, and content past the last
committed cursor is ignored after a crash. Credentials are not saved. Cached files can be browsed and
searched offline. A widened window, truncation or a changed cursor anchor starts a
fresh generation and keeps the previous one as `*.previous.log`. Caches written by
earlier versions are read as-is; error logs among them move to the timestamp scheme
on first sync. Records are matched by timestamp: continuation lines are kept with their
record when it is first fetched, but lines appended later to an already-cached record
are not re-fetched. Setting `MARKLOGIC_TUI_CURL_DEBUG` writes curl reproduction
scripts (which include credentials) under `/tmp/marklogic-tui-curl`; it is off by default.
Counters reset when switching profile/directory or restarting the application.

The TUI's own diagnostic viewer remains available via `:app-logs` or `F2`.

### Servers Interface

| Key | Action |
|---|---|
| `Enter` | Activate selected server/database/app-server |
| `j` / `Down` | Move selection down |
| `k` / `Up` | Move selection up |
| `Tab` | Cycle focus: Servers -> Databases -> App Servers |
| `a` | Add a new server |
| `d` | Delete selected server |
| `r` | Refresh databases/app-servers lists |
| `Esc` | Close popup |

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
