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
| `Ctrl+S` | Save the active query file immediately |
| `Ctrl+O` | Open the query file picker |
| `f` (when Files list is focused) | Open the tracked folder selector |
| `Esc` | Return focus to command input |

Supported launch-directory file types: `.js`, `.mjs`, `.sjs`, `.xqy`, `.sql`, `.sparql`.

- Phase 1 execution support: `.js`, `.mjs`, `.sjs`, `.xqy`
- Phase 1 editing/switching only: `.sql`, `.sparql`
- Editor changes autosave every 2 seconds after you stop typing

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
