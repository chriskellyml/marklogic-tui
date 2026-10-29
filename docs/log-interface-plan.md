# Log interface redesign — implementation handoff

Status: implementation started. The catalog, tracking modes, discovery toggle, conditional full-download checks, basic search context, and editor copy are implemented. Cursor/time navigation now uses a visible timestamp index, and the merged build uses a heap across ordered streams instead of globally sorting record references. The existing collector and viewer still buffer full responses and cached content; the streaming transport, disk-backed index and paged merge, time-range filters, archival store, and full background-service design remain outstanding.
Reviewed: 2026-10-29, against the current working tree, including existing uncommitted changes.

## Local history and archival

Retain weeks of local history, potentially several GiB. Use two storage tiers under one catalog and query index:

- **Hot:** today's partial/live files and the previous two days remain writable, with append-only raw bytes and the latest committed cursor. This tier may contain incomplete coverage and must say so in the UI.
- **Archive:** files whose last record is more than two days old become eligible only after a **verified full-file** download. A partial file or a still-changing current file is never labeled complete or compressed as an archive. Rotated physical files retain their host/path/generation identities; a day query may span multiple files, and a file crossing midnight may appear in both days' index ranges.

Store archived raw data as concatenated gzip members, each ending at a logical record boundary and containing a bounded amount of uncompressed data (start with 4 MiB, tune by measurement). Concatenated members remain readable with ordinary `gzip -dc`. A sidecar SQLite index maps `(timestamp, stable source ID, source sequence)` to `(archive generation, gzip member offset/length, uncompressed record offset/length)` and records each member's first/last timestamp. The index also stores coverage, original byte count, SHA-256, compressed byte count, and archive completion status. Compress in a background worker to a unique staging path, verify the checksum and row counts, then publish the archive generation and active index reference in one short transaction. Remove superseded hot bytes only after publication; recover or discard orphan staging data on startup.

**Thaw only what the query needs.** A page lookup uses the timestamp/source index to find record IDs and their gzip members. A bounded worker inflates those members into an in-memory LRU (or short-lived scratch files for oversized members), then formats only viewport records. Cap the uncompressed block cache by bytes, not by member count; for example 64 MiB initially. Reading a different week evicts old blocks. Full-file export may stream members sequentially to a temporary copy in the background. It must not inflate an entire day into `Snapshot.text`, and it must not replace the compressed archive with a permanent uncompressed copy merely because the user opens it.

If an archived remote file grows, shrinks, or is force-refreshed, build a new complete generation in staging and atomically switch the catalog pointer after indexing. Keep the old generation available to readers until they release it. Same-size rewrites require the explicit force-refresh path under the current size-only policy. Keep the archive deadline separate from retention: default to keeping archives until the user chooses a disk limit or deletion policy. Show compressed size, original size, coverage, and total cache usage in the UI. Cleanup should be an explicit command, with oldest complete archives removed first; never silently delete an incomplete hot tail.

This is the storage target, not a claim about the current implementation. The current `load_cache` still reads every `.log` into RAM, so enabling gzip compression alone would worsen browsing latency. Implement the paged disk reader before turning on automatic archival. Verify multi-member compatibility, random page reads across member boundaries, continuation records, huge individual records, interrupted compression, and archive replacement while readers are active.

## Outcome

Show remembered logs immediately, make full downloads on demand the normal workflow, keep live collection optional, and browse/search/merge local files without retaining the complete corpus in RAM. Network, disk, indexing, and search work must run independently of terminal input and rendering.

## Findings in the current implementation

These are code findings, not measured production profiles. Capture timings before claiming a specific speedup.

| Symptom | Cause and implementation reference |
| --- | --- |
| Remembered files disappear | `LogViewer::is_file_visible` checks the history window **before** subscription state. `is_file_in_history_window` hides zero-byte files and files last modified before today's midnight (`src/logs.rs:836`, `:866`). Existing tests explicitly enforce hiding stale tracked files. |
| Tracking survives but the list can be empty | `subscriptions.cfg` stores filename rules, but host/path rows are restored only from downloaded snapshot JSON. Inventory without downloaded content is not persisted. `open` derives endpoints from those rows and cannot bootstrap an empty cache (`src/logs.rs:361`, `:467`). |
| Enter is unpredictable | Enter initiates incremental collection rather than simply opening local content (`src/events.rs:1939`). Requests can silently return when the single global `busy` flag is set. The selected, cached, untracked file can also enter automatic polling through `plan` (`src/logs.rs:1702`). |
| Collection feels like foreground work | HTTP runs in Tokio tasks, but `tick` applies results, writes files, updates in-memory text, and rebuilds views on the terminal thread (`src/logs.rs:1820`, `:1924`). `resize_content`, called during rendering, can rebuild the entire combined view. |
| Collection stops when leaving logs | `close` cancels the collector; the timer requires the logs screen to be visible and `following` to be enabled. View position, collection policy, and screen visibility are coupled. |
| Slow incremental reads | Error logs use timestamp scans with up to 50 rounds of 20,000 lines per stream. Other logs read the entire remote file before returning a small character suffix (`src/logs/collect.xqy`). This is bounded transfer, **not** a remote byte-range read. |
| Discovery competes with syncing | A global busy flag serializes commands, discovery visits hosts sequentially, and every twelfth poll runs discovery again (`refresh`, `tick`). The inventory query's modified-since cutoff also affects explicit file downloads, so an older file can be reported missing. |
| Large memory footprint | `load_cache` reads every committed `.log` into `Snapshot.text`. Raw lines, parsed records with raw/message copies, styled rows, and plain rendered strings add further copies. `logs_eval` also buffers complete response bodies (`src/client.rs:408`, `:431`). |
| Merge is expensive independently of download policy | `rebuild_combined` updates parsers, gathers all candidate records, sorts the complete set, formats every record, and rebuilds rows/strings/match vectors. Filtering, wrapping changes, and updates repeat much of this work (`src/logs.rs:1356`). Parsing appended data is already incremental; global sorting/rendering is not. |
| Search blocks while typing | Every character filters the full raw view or rebuilds the combined view. Time seeking finds a position, rather than imposing a time filter. There are no before/after context controls. |
| Editor/export copies the entire result | `V` builds one large string, writes it, and reads the editor result back. A path-based editor helper already exists (`src/external_editor.rs`). |

Focused baseline command:

```sh
cargo test logs -- --skip live_ --skip client::tests::test_
```

Current result (2026-10-29): **81 passed, 6 failed** with this focused command. Failures are `opening_cached_logs_checks_every_discovered_host_even_after_a_failure`, `selected_file_shows_each_hosts_size_and_cache_age`, `empty_tracked_logs_are_gray_and_modes_are_explained`, `discovery_screen_shows_the_day_and_navigation_hint`, `selected_host_details_can_scroll_without_hiding_the_file_list`, and `long_hostnames_are_readable_below_compact_file_details`. These failures were present before the cursor and merge optimizations in this update. Several fixtures use 2026-10-14 while visibility and pruning use today's date; fix clock determinism before using this suite as a strict regression baseline.

## Product behavior

### Four visible modes

| Mode | Default visibility | Remote behavior | Suggested appearance |
| --- | --- | --- | --- |
| Tracked live | Always listed, even empty, stale, missing, or never fetched | Poll about every 5 seconds; incremental tail where supported | `[L]` cyan/bold |
| Tracked on demand | Always listed, even before its first download | An explicit key checks remote size; download the complete file only when needed | `[D]` green/bold |
| Partial | Keep the explicitly previewed file listed | One bounded preview on request; no periodic collection | `[P]` yellow; show coverage |
| Untracked | Hidden unless discovery is expanded with `r` | Metadata discovery only; no automatic content download | `[U]` dim |

Use letters as well as colors. Show busy/error/offline/stale indicators separately; a request in progress does not change the file's mode.

Interpretation of “partial” for this plan: a retained, one-off preview of a file, with no automatic refresh. This is a proposed meaning because the request does not define its lifecycle precisely. Prefer a recent bounded time window for ErrorLog previews. Keep any existing character-suffix preview confined to this explicit action, label its whole-file server read cost, and never treat it as the default full-download strategy. Do not invent a remote range-read guarantee.

Store **mode and cache coverage separately**. For example, an on-demand file may have only a preview cached; that must not satisfy the full-file change check. Coverage includes none/full/partial, generation, committed bytes, and any known time range or gaps.

### Entry, discovery, and selection

- Opening logs loads a small persistent catalog and tracking rules immediately. Do not read the whole corpus to populate the sidebar.
- A tracked row's visibility does not depend on modified date, nonzero size, or the current content time filter. Show `not downloaded`, `empty`, `offline`, or `no records in this time range` as appropriate.
- Persist discovered hosts, effective directories, host/path identities, and last-known metadata independently of content. Preserve current filename-wide tracking across all hosts; expose host-specific overrides without accidentally changing every host when selecting one row.
- On first use, discover host names/effective directories and resolve only tracked names. Render configured tracked-name placeholders until resolution finishes. Background bootstrap may find tracked files without displaying unrelated filenames.
- `r` switches discovery on and refreshes metadata without a date cutoff; the next `r` hides untracked rows immediately. It does not download their contents. A late discovery result must respect the latest hidden/expanded state. `R` can explicitly refresh again while expanded.
- `F` filters the rows already allowed by the current visibility state. It must no longer reveal untracked names by running discovery implicitly. Stop automatic full inventory refreshes on live polls; checking known tracked paths and refreshing host topology is separate from discovering unrelated files.
- Keep selection by stable identity, not array position. Stable host aliases come from the catalog rather than the currently visible subset.
- `Enter` opens cached content and moves focus. It performs no hidden download. An uncached row explains the available explicit actions.

### Proposed keys

| Key | Action |
| --- | --- |
| `o` | Choose live / on demand / partial / untracked in a small mode picker; default scope is the selected host/file, with an explicit all-hosts option |
| `s` | Refresh all tracked-on-demand files now; also request an immediate catch-up of live files |
| `d` in file list | Refresh the selected tracked file according to its mode; for partial, refresh the preview; for untracked, explain how to choose a mode |
| `b` | Force a full download of the selected file, including when its size is unchanged; an untracked/partial row becomes tracked on demand |
| `r` / `R` | Show-and-discover / hide untracked rows; explicitly rediscover while expanded |
| `f` | Follow the bottom of the current view; does not enable/disable network collection |
| `x` | Cancel the selected job, or choose a running job when several exist |
| `m` | Toggle the chronological local merged view; does not trigger downloading |
| `/` | Search/filter panel shared by single-file and merged views |
| `V` | Open a stable raw snapshot or filtered/merged export in `$EDITOR` |

Provide a visible pause/resume-live control in the job/mode menu. Retain existing paging, port/type filters, error navigation, and wrapping where they fit these semantics. Update help and README together; do not leave the old meaning of Enter/f/s in documentation.

Proposed default: new tracking uses **on demand**. Migrate old enabled subscriptions to on demand, retain explicit disabled rules, and retain the three standard ErrorLog names as initial on-demand rules. This changes the former automatic default intentionally, matching the preference for explicit full downloads. Existing partial caches remain available and visibly incomplete until refreshed.

## Collection architecture

### Worker ownership and lifecycle

Split the current large `LogViewer` into a UI model, a log service, a remote adapter, a disk store/index, and a paged local query layer. Suggested modules: `logs/catalog.rs`, `service.rs`, `remote.rs`, `store.rs`, `query.rs`; retain `presentation.rs` formatting logic behind a bounded record API. These are boundaries, not a requirement for a generic framework.

- The service owns collection policy, scheduling, deduplication, and per-file status. Tokio handles network I/O; a dedicated blocking writer owns SQLite ingestion and buffered file parsing, with separate bounded read workers/connections for browsing. Use WAL and short, batched staging transactions so indexing a large file cannot monopolize the reader or delay live commits for every other file. Never run large parsing/indexing work directly on an async executor thread or the terminal thread.
- Live collection continues while the app is running for the active profile/directory, including outside the logs screen and while an external editor is open. No OS daemon. Switching connection/directory cancels old remote jobs and rejects late results using a context generation token.
- Use a timer independent of screen drawing. Poll on a roughly 5-second cadence, skip/coalesce overdue ticks, never overlap collection for the same file, and back off failing files individually.
- Replace global `busy` with per-file jobs and a bounded queue. Start with at most four remote jobs and a per-host limit; make limits tunable after measurement. A large download must not prevent other files' live polls or local browsing.
- Repeated `s`/`d` coalesces pending work instead of silently dropping it or downloading duplicates. Mode changes cancel incompatible work and invalidate results through a per-file policy revision.
- Send bounded notifications containing metadata, progress, and committed page/version IDs. Do not queue full log strings in an unbounded channel. Limit UI event processing per tick so completions cannot starve keyboard input.
- Report per-file states: queued, checking, downloading (bytes), indexing, ready, unchanged, changed during read, offline, error, cancelled. Show last checked/downloaded times and aggregate progress without blocking the screen.

### On-demand full download protocol

1. On `s`/`d`, fetch metadata for requested host/path pairs, batching checks per host. Do not exclude files based on day or modification time. An unchanged check must never read remote file contents.
2. Compare the remote byte size with the size of the **last successfully verified full download**, not with last-seen sidebar metadata or partial-cache size. If there is no full snapshot, fetch it even if size is zero. Otherwise equal size means unchanged, per the requested policy.
3. If different, fetch the complete selected physical file once. Do not use timestamp pagination or the character-suffix catch-up loop for on-demand files. Full means all file contents; time filters apply locally afterward. Rotated files are separate selectable files, not silently concatenated into that snapshot.
4. Stream the response into a unique staging file; parse and index incrementally with bounded buffers. Keep serving the last committed generation while staging is incomplete. Preserve UTF-8, CRLF, blank lines, and final unterminated lines.
5. Check remote metadata around the read and validate payload completeness. If it changed during transfer, retain a valid captured snapshot with a `changed during read` status, but do not mark the current remote version as verified unchanged. A later **explicit** refresh rechecks/replaces it. Avoid endless automatic retries on busy logs.
6. Publish the completed generation through one committed manifest/index change, then notify the UI. A network failure, parse/index error, cancellation, or stale result must leave the previous visible generation and verified baseline intact.

Size-only comparison intentionally misses same-size rewrites/rotations. Keep modified time as diagnostic metadata and offer `b` to force a refresh. Do not quietly change the default into a different policy. A later optional strict size+mtime policy can address that tradeoff.

**Transport gate:** the present JSON-in-multipart `logs_eval` buffers whole responses and cannot deliver bounded client memory. Add a log-specific streaming path over the existing authenticated `/v1/eval` route, ideally a small metadata part plus an unescaped text payload. Parse multipart framing across arbitrary HTTP chunk boundaries; reuse authentication, timeout/cancellation, TLS, and byte accounting. A streamed JSON string decoder is an alternative only if it is demonstrably simpler and bounded. Test the selected framing against a real server before building the rest of full downloading around it. Never use `.bytes()`, `.text()`, or a whole-file `serde_json::Value` in this path.

Streaming HTTP transport is different from repeatedly reading the server file for suffix chunks. `xdmp:filesystem-file` returns a UTF-8 string and provides no offset parameter; client streaming does **not** establish bounded server memory or eliminate its whole-file read cost. Preserve routing through `file://host/...` and the existing eval endpoint so tunnelled profiles continue working. Add a download-specific timeout/idle policy rather than assuming the existing 30-second evaluation timeout suits large files. If transport cannot meet the memory requirement, report that blocker explicitly; do not ship the existing buffered path as the final solution. [MarkLogic filesystem-file reference](https://docs.marklogic.com/xdmp:filesystem-file).

### Live and partial paths

- Keep the ErrorLog timestamp scanner as the experimental live adapter. Commit small batches as they arrive, with byte/time budgets as well as line limits; do not accumulate 50 rounds into one in-memory event.
- Re-read a small overlap and reconcile the final record so later continuations are not lost. Preserve legitimate identical records. Detect no-progress timestamp pagination, truncation, midnight rotation, and interrupted reads; expose gaps rather than claiming a complete tail.
- `xdmp:logfile-scan` is specific to ErrorLog format and limits **lines**; do not treat it as a universal byte-tail API. Retain on-demand full download as the reliable fallback. [MarkLogic logfile-scan reference](https://docs.marklogic.com/xdmp:logfile-scan).
- For Access/Request/Audit files, initially offer on demand and partial. If live is requested, explain the adapter limitation and require an explicit choice of the expensive experimental fallback; never silently poll whole files under a “tail” label.
- Preview is an explicit one-off operation with known coverage. Switching a partially cached file to on demand requires its first full download even when the remote size matches the preview's last metadata.

## Disk store, indexing, and local queries

Use raw UTF-8 files as the authoritative content plus a local SQLite index. A bundled SQLite Rust binding keeps this within the single-binary app; verify FTS5 support in the chosen build. This is local infrastructure, not a new server/service dependency. Do not move the full corpus from `Vec<String>` into one database query result vector.

Suggested persisted data:

- Catalog: connection identity, host, effective directory, path/name, mode rules/overrides, availability, last-seen remote size/mtime, last-check status.
- Generations: immutable raw path, committed bytes, coverage, verified full-download size, live cursor, active/staging state, parser/schema version.
- Records: stable ID within generation, source ID, source sequence, byte range, start/end physical line numbers, timestamp and timezone provenance, kind, level, port, searchable text. Raw bytes stay in files; normalized searchable text is allowed on disk, not retained corpus-wide in RAM.
- Ordering indexes for timestamp plus deterministic source/sequence ties, and source/generation/sequence for file views and context lookup. Keep active-generation lookup indexed. Confirm query plans with the actual filter shapes.
- A trigram FTS5 index over searchable text for substring candidate selection. With an external-content table, update records and FTS consistently in the same transaction. Verify exact literal/case behavior against candidates; escape query syntax so punctuation remains literal. Short terms and general regex need a bounded background scan fallback. FTS must not silently turn substring searches into word searches. [SQLite trigram and external-content documentation](https://sqlite.org/fts5.html#the_trigram_tokenizer).

Store UTC timestamps when the source carries an offset; record how naive server timestamps were interpreted and render in server time. Do not pretend a single fixed offset resolves historical DST ambiguity. Keep raw text and flag unknown/ambiguous timestamps.

Single-file and merged views share one query specification: sources, active generations, keyword/regex terms, include/exclude terms, host/port/kind/level, time bounds, context, and presentation options. Use fixed-size pages and keyset cursors, not deep SQL OFFSET or a vector containing every matching ID. Keep only viewport pages, a small prefetch margin, and a byte-capped formatted-record cache. Bound SQLite cache, transfer buffers, and queues too. A huge single record/line must spool to disk and render in slices rather than defeating the cap. Apply the same bound to searchable text and XML/JSON formatting: use indexed text chunks mapped to their logical record, verify matches across chunk boundaries, and provide raw sliced output when structured formatting exceeds its budget. Specify and test regex limits explicitly if the selected engine cannot scan oversized records incrementally; do not quietly omit them or load them whole.

Full-file replacement stages raw data and index rows, then atomically switches active generation. Live ingestion appends raw bytes and commits index/cursor metadata together; recover to the last committed byte boundary after interruption. Keep old files until no reader/export references them. Reconcile orphan staging files on startup. Preserve recoverability without accumulating unlimited `.previous.log` generations; expose explicit cache retention/cleanup separately from display filters.

Import existing snapshot metadata and committed raw prefixes in the background, without downloading again. Do not call the current eager `load_cache` or prune history merely because the screen opens. Keep the old cache until migration completes; an interrupted migration must restart safely. Tracking/catalog data survives “clear content.”

## Search and time filters

- `/` opens one filter panel for both raw and merged views. Keep the existing literal and `/regex/i` alternatives while making mode explicit. Apply/debounce in a worker (around 200 ms); cancel superseded queries and discard stale pages by query version.
- Offer **last N minutes**, **from**, **until**, and before/after context controls. Use an inclusive lower bound and exclusive upper bound. Accept server-local datetimes and explicit offsets; show the interpreted range in the status bar.
- “Last 15 minutes” is a local rolling filter based on server-clock calibration, not a request for remote collection or deletion of older cached data. Update its bounds while following. Absolute ranges remain fixed. Offline mode shows the time basis used.
- Apply indexed source/time filters before text evaluation. Build candidate IDs/results on disk if a regex scan must materialize many hits. First-page availability and exact total count may arrive separately; show progress and permit cancellation.
- Search whole logical records so ErrorLog continuation stacks remain attached. Context controls count **physical lines** before/after matched records within each source, then expand boundary records to avoid chopping stacks. Deduplicate overlapping spans and mark context separately from actual hits. In merged view, combine the resulting source-local spans chronologically.
- Time/source/port/type restrictions remain hard bounds on context. Unknown-time records remain visible without a time filter; with one, exclude them by default and expose an explicit “include unknown timestamps” choice/count.
- Search raw content plus consistent metadata/normalized display fields; formatting toggles must not silently change which records match. Compile patterns once per query. Navigation requests the next matching/error record from the index.
- Opening a filtered local cache never triggers a download to fill missing time coverage. Show coverage limitations and the explicit refresh action.

## Fast merge

The merged view becomes an ordered query over indexed **records**, not a new combined text blob. Include live and on-demand tracked sources by default; partial sources may be added explicitly with a coverage marker. Discovery visibility must not change merged membership. Preserve the existing all-cached option as an explicit scope.

- Order timestamped records by `(timestamp, stable source identity, source sequence)`. Keep real duplicate records and multiline records intact. Untimestamped records get a clearly defined separate group; continuations inherit their parent record's timestamp.
- Fetch only the visible page and nearby records. Format, XML-decode, highlight, and wrap those records only; cache by record/generation/options/width. Do not format every nonmatching record just to decide whether it matches.
- New live records update the disk index and invalidate affected pages. Late records insert in the correct chronological place; never assume all streams append in global timestamp order.
- Anchor a reviewing viewport by record ID and intra-record line position, rather than row number or nearest timestamp. Preserve anchors across inserts; after full replacement, resolve the closest source/time/sequence anchor and indicate if its original record disappeared.
- Rendering and resize handlers only consume ready pages. Changing width invalidates bounded wrapping data and requests replacements; it does not scan or sort the corpus.

## Editor and export

Retain `V`, reusing `open_file_in_external_editor(path)` rather than the text-edit/readback helper. Give the editor a stable local snapshot/copy of the selected raw generation, or stream the current filtered/merged query into an export file in a worker. Offer raw vs current view in a small picker. Do not allow editor saves to corrupt authoritative cache/index files, and do not read the result back into RAM. Hold a generation/query snapshot for consistent exports while collection continues. Cancellation removes incomplete exports; the terminal suspension/resume behavior remains shared with existing editor support.

## Implementation sequence and acceptance gates

1. **Baseline and behavioral model.** Add deterministic clocks, capture release-build startup/search/merge/ingestion timings and peak RSS, define catalog/mode/coverage types and migration. Fix always-visible tracked rows, persistent inventory, Enter, `r` toggle, and appearance. Update conflicting tests. Gate: rows survive restart without any downloaded text; no untracked row appears until `r`.
2. **Service and remote protocol.** Remove global busy/follow coupling; implement bounded per-file jobs, cancellation/version tokens, progress, five-second scheduling, batched metadata checks, and conditional full download. Prove the streaming transport on a real cluster before integrating large downloads. Gate: unchanged files cause zero content reads; changed files use one full transfer; typing/navigation remains responsive while jobs run.
3. **Disk-backed ingestion and single-file reader.** Add transactional generations, record/line offsets, bounded parsing, background cache migration, and paged raw/formatted reads. Gate: remove whole-corpus `Snapshot.text`, `lines`, `rows`, `parsed_streams`, and `matches` ownership from production paths; enforce caps on unusually large records and queued events.
4. **Indexed search and merged reader.** Add shared query/time/context semantics, FTS candidate selection plus correct scan fallbacks, keyset paging, viewport formatting, and stable anchors. Gate: merge/search does not globally materialize/sort/render all records on the UI thread; behavior is identical across raw/merged search modes.
5. **Editor, recovery, documentation, performance verification.** Add streaming consistent export, safe editor snapshots, retention controls, crash recovery checks, migration/offline tests, and updated help. Compare release benchmarks to step 1; record measurements and remaining server limitations.

Do not present step 1 or merely moving the existing whole-file vectors into a worker as completion. The bounded-memory store and paged query path are essential deliverables. Prefer these reviewable stages over rewriting presentation parsing and collection simultaneously.

### Required verification

- Catalog/mode matrix: cold start, empty/quiet/old/missing logs, settings without snapshots, offline restart, discovery while syncing, hide during discovery, filename-wide rules and host overrides, mode changes during downloads.
- Conditional downloads: unchanged size, no prior full cache, zero-byte file, partial cache with equal metadata, growth, shrink, same-size rewrite + force refresh, rotation during transfer, transfer/index failure, cancellation, and byte/character distinctions.
- Live: five-second timer with fake time, slow-job coalescing, leaving the screen, profile switch, editor suspension, isolated host failure, late continuations, identical timestamps, no-progress scan pagination, and midnight rotation.
- Transport: digest/basic auth, arbitrary multipart/chunk boundaries, exact UTF-8/CRLF/blank-line preservation, oversized lines, error bodies, network cancellation, timeout, and response-byte accounting. Real-server smoke tests remain opt-in and read-only.
- Query/merge: mixed source kinds/offsets, missing/ambiguous timestamps, multiline stacks, literal punctuation/short terms/Unicode/regex, all terms vs alternatives/exclusions, context overlap, hard time boundaries, stable ordering, late inserts, wrap/resize, search cancellation, and raw/merged consistency.
- Storage: crashes before/after raw writes, index commit and generation publication; old readers during replacement; interrupted migration; committed-prefix recovery; FTS consistency; content clearing preserves tracking and catalog.

### Performance targets to validate, not current claims

Use generated, reproducible datasets: 20 sources, at least one million records, 1 GiB then 10 GiB total raw text; include an enormous record and delayed/out-of-order arrivals. Record hardware, release settings, warm/cold cache, download time separately from indexing, total disk overhead, and p50/p95 UI latency.

- Tracked sidebar appears within 250 ms and the warm first content/merged page within 1 second, without scanning the corpus on entry.
- Typical input-to-paint p95 stays below 150 ms during downloads, indexing, search, and live updates; no single completion/render handler performs a corpus scan.
- Additional log-service RSS stays within a proposed 128 MiB budget as the corpus grows from 1 to 10 GiB; demonstrate the bound under full transfer, giant records, all-match queries, and many queued updates.
- Indexed warm time/keyword first pages target under 500 ms. General regex may require a scan: require bounded memory, early progress, cancellation, and measured throughput rather than promising constant time.
- Reopening merge, resizing, and receiving a small live batch must not redo work proportional to the full corpus. Inspect query plans and profiles if page fetches still sort large candidate sets.

## Terra handoff brief

Implement this plan in the existing working tree while preserving unrelated edits. Prioritize on-demand full-file sync, always-visible remembered rows, and the disk-backed paged reader. Keep live scanning experimental and isolated. Make the proposed mode/default/key choices explicit in the implementation notes. Report baseline versus final measurements and any real-server transport constraints; do not claim server-side range reads or bounded server memory from client streaming. No commit, publishing, or separate chat creation is required by this plan.
