# MarkLogic Workbench desktop preview

A Tauri 2 desktop application with React, TypeScript, Vite and CodeMirror. Both
the terminal app and desktop app use `crates/marklogic-core` for MarkLogic
connections, authentication, configuration, query files and XQuery imports.

## Run

Install Node.js 22.12+ (or a newer supported LTS), Rust, and the platform's
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

From this directory:

```sh
npm install
npm run tauri dev
```

`npm run dev` runs only the frontend in a browser. Native server profiles, file
access and query execution require the Tauri app. The browser displays a message
explaining how to launch it.

Build the optimized desktop application for normal use:

```sh
npm run tauri build -- --bundles app
```

On macOS it is created at `../target/release/bundle/macos/MarkLogic Workbench.app`.

The packaged macOS app has an **Update app** button in its title bar. It builds
the current local checkout (including uncommitted changes), installs the newly
built bundle in the same location, then automatically opens the updated app and closes the old one. If the query has unsaved edits, it asks before discarding them; you can save and choose **Restart updated app** to finish.
It does not pull from Git. The checkout, Node/npm, Rust and Tauri prerequisites
must still be available on that machine. The first update uses a separate build
directory and may take several minutes. Development-mode and other-platform
instances cannot update themselves this way.

The existing terminal commands still work from the repository root:

```sh
cargo run
cargo build --release
```

The workspace's default members exclude the desktop app, so building the TUI
does not require frontend tools or desktop webview libraries.

## First version

- Loads the TUI's existing `config.toml`, including its saved active server,
  app server, database and query variables. Use **Server** to add or edit profiles
  and their app-server endpoints.
- Existing passwords stay in the local configuration and are not sent to the
  frontend; enter a new password in **Server** only when replacing one.
- Allows selecting an app server and typing a database name. **Connect** discovers
  databases through the Manage API; query execution can also run without discovery
  when the account does not have Manage API access.
- Opens the default query store or a folder chosen with the native folder picker.
  Saved TUI folders are also available. Supported files directly in the selected
  folder are included in the desktop explorer.
- Edits existing files and saves with Cmd/Ctrl+S. A save is rejected if the file's
  contents changed on disk after opening. Only supported query files within the
  selected folder can be opened or saved. The editor accepts files up to 8 MiB.
- Runs current editor contents with Cmd/Ctrl+Enter. JavaScript and XQuery execute
  through the shared client. Saved query variables and local XQuery import
  expansion are supported. SQL and SPARQL files can be edited and saved.
- Provides scratch queries for JavaScript and XQuery. Copy scratch contents to a
  file before closing; creating files and Save As are future work.
- Shows plain text/XML or formatted JSON results, 50 items per page. Results are
  rendered as text, including HTML returned by MarkLogic.
- Confirms before discarding unsaved changes on navigation or native window close.

Desktop selections are session-local. The app reads the existing profile and
tracked-folder configuration; query saves write to the selected source file.

Database discovery times out after 15 seconds; query requests after 120 seconds.
A timed-out query may continue executing on MarkLogic. Query cancellation is not
implemented in this preview.

## Log grid

The **Logs** workspace uses a port × calendar-day × Error/Access/Request matrix.
It opens remembered local files immediately; **Discover** lists remote
files without downloading their contents.

- Each row is a port, with ROOT and TASK rows for the corresponding MarkLogic logs.
  Each day has narrow E, A and R subcolumns. Color shows complete, partial/stale,
  remote-only or empty files across the hosts.
- The seven-day grid stays visible while you read; **Files** or `\` hides or
  restores it. Click a cell to select a port/type/day across hosts. **V**, or the
  port label, selects that port and type across all days. **C**, or an E/A/R
  column header, selects that day and type across visible ports. A day header
  selects all types and ports for that date.
- Arrow keys and h/j/k/l move through the matrix. Page Up/Down changes the day
  while retaining port/type. Space or Shift-click selects multiple cells.
- The +/− mark beside a port cycles include, exclude and clear. The grid filter
  narrows ports by port number, log name or host. Older/Newer and the date picker
  move the seven-day window.
- Calendar selection filters records inside a physical file. Files spanning
  midnight can appear in multiple day columns. Existing TUI record indexes,
  partition manifests and the desktop index supply known historical dates;
  remote rotation numbers provide inventory date hints.
- Search, include/exclude, error-only filters and context operate on the selected
  grid scope. Host logs occupy full-width horizontal bands stacked vertically,
  each with a narrow activity strip on the same time axis. **Maximize** or **z**
  focuses the current host. Linked scrolling follows the leading host’s time.
- **Merge** or **m** combines the current port’s error and access logs through
  the same chronological merge iterator as the TUI. Row, column and day scopes
  also merge their selected sources. Request logs remain separately selectable.
- Click a host log or press Enter on the grid to focus its entries. **j/k** or
  up/down move the highlighted entry; **u/d** and Page Up/Down move by a screen,
  **g/G** jump to the first/last entry in the current filtered time window,
  loading the appropriate page, **h/l** scroll horizontally, and **H/L**
  change the leading host. **#** cycles the gutter through off, source-file
  record numbers and overall positions. Overall positions follow chronological
  order within the current filtered scope, across all hosts in merged view and
  within each host in split view. The number button and Options menu offer the
  same choices. Hover over an overall number to see its source-file record number.
  Host colors match across the host
  list, pane headings, activity strips and merged aliases. **v** opens or closes the selected entry's detail;
  in merged view it opens or closes the options panel. Expanded details stay open during
  Live refreshes; opening one pauses Follow so it stays in view. The detail wraps,
  formats valid JSON or XML after the log prefix, and its **Copy** button copies the
  displayed text. Invalid or mixed-content payloads retain their original form.
- **fi/fo**, or the **Filter in/out** buttons, open a movable, non-modal rule
  editor. Drag its header to move it; it becomes 75% transparent away from the
  pointer. Paste one term per line and choose **Add terms**, or **Apply** to add
  pending terms and close. No `-F` or manual delimiter is needed. Commas, pipes,
  whitespace and capitalization inside each term are preserved; automatic
  capitalization, correction and spellcheck are disabled. Ctrl+U clears the
  bulk-entry box; Ctrl/Cmd+Enter applies. Escape restores the opening rules.
- Each term has an enabled checkbox, editable text, field, match mode (contains,
  exact value or regex) and case mode (ignore, match or smart case). Fields are
  any field, record text, host, filename, port, HTTP method, level and log type
  (`ERR`, `ACC`, `REQ`). Method is extracted from the access request or request
  JSON's `method`/`httpMethod` when available. Missing methods never match a
  method rule. Regex uses Rust's regex syntax, including inline flags such as
  `(?i)`, without slash delimiters; invalid patterns show an error and prevent
  applying. Look-around and backreferences are not supported.
- Enabled keep rules accept **any** matching term; enabled drop rules remove
  **any** matching term and take priority. With no enabled keep terms, all records
  are eligible. Rules apply to entire records, counts, timelines and context,
  across pages. Enable/disable/remove all affects the current keep/drop tab.
  **fc** clears all rules and search, preserving the time window. A standalone
  **f** toggles following after the TUI's 700 ms command delay. The TUI's existing
  smart-case and `-F` syntax is unchanged.
- Saved lists contain both keep/drop rules, including field, matching options
  and disabled states. Save a new named list, update or rename it, load it to
  replace the current rules, append it, or delete it. Appending deduplicates
  identical rules. Lists persist in this desktop installation's local webview
  storage across restarts; they are independent of TUI presets. Cancel restores
  the opening rules, while explicit saved-list changes are retained.
- Select text in a log record and right-click to include or exclude that literal
  text with its original case. Right-click any record for exact **Keep/Drop**
  actions on its port, HTTP method (when present), host, level or log type.
- **t** and **T** set the window's start and end to the selected entry; **tt**
  centers a five-minute window and **U** undoes the last keyboard window change.
  A number before t/T moves that bound back by that many minutes; a number
  after t/T moves it forward. Ctrl+T opens the time controls.
- The viewer always shows earlier controls, the selected or last centered time,
  and later controls. Each side expands by 1, 2, 5, 10, 20, 50 or 100 minutes;
  **All** removes that side's limit. **Live** on the later row clears the end
  limit and follows current remote logs. These controls remain usable when no
  records match; the grid's selected days and content filters still apply.
- Server and app-server selection live in the toolbar’s compact server popover.
  Display options and time controls also open over the logs.
- Right-click an entry to hide earlier or later logs, center or adjust a time
  window, clear or undo it, or inspect the entry.
- **Download** (or **d** while focused on the grid) queues the selected files in full across enabled
  hosts; **Tail** refreshes their latest content. If needed, these actions
  discover remote files first. Both show
  progress and allow cancellation. The grid marks queued cells with Q and
  active downloads with a down arrow. **Live** refreshes selected current streams every
  five seconds while Logs is visible. Automatic live requests use `/v1/eval?orig=tui`, so they can be filtered from access logs. Each selected file produces a request; digest authentication adds a challenge request. A failed refresh stops live polling.

Both apps share collector scripts, host validation, record parsing, compact
formatting, chronological merging, gzip readers and partition readers. The
desktop reads committed TUI cache data and builds its own disk-backed record index. New desktop generations
live under the cache's `desktop/` directory. Earlier desktop generations are
retired after publication where possible; the TUI's source files are retained.

The first query of a changed source builds its index in the background. The
desktop session retains filtered record keys, counts and activity data while
paging, so jumping to the beginning or end does not repeat the text scan.
Existing log rows remain visible while a new filter is evaluated. View responses
are capped at 100 records per host (200 in merged mode) and approximately 4 MiB;
individual source lines over 4 MiB or multiline records over 16 MiB produce an explicit indexing error.
Grid selections and port marks currently last for the desktop session. TUI preset sharing, storage maintenance actions and automatic historical archival
have not yet been exposed in the desktop interface.

## Checks

```sh
npm run build
npm test
cargo check -p marklogic-desktop
cargo test -p marklogic-desktop --lib
```

The Rust commands can run from either the repository root or `src-tauri`.
The core and TUI tests run with `cargo test` from the repository root. The core
inherits existing live-server tests that require a local MarkLogic instance with
the `data-platform-content` fixture database.

## Next steps

Add file creation and Save As, profile editing, document browsing and reports.
Add saved-list import/export, TUI preset sharing and desktop storage maintenance.
The report engine still uses `logs::visit_cached_error_logs` in the terminal app.

Keep frontend Tauri calls behind `src/api.ts` and `src/logs-api.ts`. A future
browser deployment could implement those interfaces using a Rust HTTP service.

## Fused query and module workspace

Files and downloaded modules share one explorer, editor and results pane in
**Queries**. The compact environment menu selects server, app server, content
database and modules database. Filename filtering preserves folder hierarchy.

Under **MODULES**, enter a URI in **Search modules** and select **Search** to find server URIs. This field also filters downloaded modules and is separate from the file filter. Download individual modules or select results for bulk download. Downloads
keep existing local edits. Open any downloaded module in the shared editor.

Unsynced modules have amber badges and a persistent **UNSYNCED MODULE** bar showing
the exact server, database and URI, with **Sync** and **Drop** icon actions. Sync is
available immediately: after destination confirmation it saves current edits and
pushes them. Compare is optional. Remote changes since checkout reject the push.
Drop asks for confirmation, downloads the current server copy, and discards local
edits. Sync errors retain the pending state. Save locally never clears that state.
Switching from an edited module to another file saves its working copy locally;
it stays pending until synced or dropped. Modules are library documents: their tabs show editing and sync controls, with no
Run action or results pane. Local query tabs retain normal execution and results.

Working copies share the TUI cache under the selected folder's
`.marklogic-tui/module-environments/`, separated by server, endpoint and user.
The module browser does not poll. App close and update restart protect unsaved edits.

### Editor tabs and local autosave

Queries and modules open in closable tabs. Switching tabs retains each tab's text,
module destination, comparison state and query results. Closing an edited file tab
saves it locally first; failed saves keep the tab open. Module tabs remain marked
unsynced after local saves. Sync remains an explicit confirmed action.

After two seconds without typing, changed file/module tabs save locally, including
background tabs. Writes are serialized per tab, preserve newer edits that arrive
while a write completes, and check the previous on-disk contents. Failed autosaves
remain dirty and report the error; the same text is not retried continuously.
Scratch drafts save in local webview storage and reopen after restarting the app.

Logs and Queries share the connection picker at the right of the top bar. Click outside it or press Escape to close it. The file sidebar defaults to
450 px, can be resized with its divider (drag or arrow keys; Home resets), and
remembers its width. Editor text uses a fixed 12 px font independent of UI scaling.
