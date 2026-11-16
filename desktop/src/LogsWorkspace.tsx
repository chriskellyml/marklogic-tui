import { memo, useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { ArrowDownToLine, ChevronLeft, ChevronRight, LoaderCircle, Pause,
  Play, RefreshCw, Search, SlidersHorizontal, X } from 'lucide-react';
import { errorMessage, type Connection, type ServerProfile } from './api';
import { logsApi, type LogFile, type LogPage, type LogProgress, type LogRow, type LogView } from './logs-api';
import { LogArchivePanel, defaultArchiveSettings } from './LogArchivePanel';
import type { ArchiveSettings } from './logs-api';
import { LogGrid } from './LogGrid';
import { formatLogDetail } from './log-detail';
import { LogFilterDialog } from './LogFilterDialog';
import { appendRules, newRule, type FilterAction, type FilterField, type FilterRule } from './log-filters';
import { currentServerDay, defaultGridSelection, selectedSources, scopeLabel, rotationOf, portLabel, type GridSelection, type PortMarks } from './log-grid';
import './logs.css';
import './log-grid.css';

interface DownloadRequest {
  id: number; session: number; selection: GridSelection; marks: PortMarks;
  hosts: string[] | null; files: LogFile[]; full: boolean;
}

interface Props {
  embedded?: boolean; preferredPort?: string; refreshInterval?: number;
  active: boolean; servers: ServerProfile[]; connection: Connection; disabled: boolean;
  onServer: (name: string) => void; onAppServer: (name: string) => void;
}
const HOST_COLORS = ['#83d5ff', '#ffd477', '#c5a9ff', '#8ce4ac', '#ff9f9f', '#ffb778', '#eda8de', '#72ddd0', '#d9de8e', '#a9bdff', '#ffb9ca', '#dec090'];
export function LogsWorkspace({ active, connection, embedded = false, preferredPort = '----', refreshInterval = 5000 }: Props) {
  const workspaceElement = useRef<HTMLDivElement>(null);
  const [port, setPort] = useState(preferredPort);
  useEffect(() => { setPort(preferredPort); }, [preferredPort]);
  const [gridMode, setGridMode] = useState<'expanded' | 'hidden'>(embedded ? 'hidden' : 'expanded');
  const [reading, setReading] = useState(embedded);
  const [leadingHost, setLeadingHost] = useState('');
  const [maximized, setMaximized] = useState(false);
  const [view, setView] = useState<LogView | null>(null);
  const [health, setHealth] = useState<'checking' | 'connected' | 'disconnected'>('checking');
  const [serverTime, setServerTime] = useState('');
  const [clock, setClock] = useState(Date.now);
  const [directory, setDirectory] = useState('');
  const [directoryDraft, setDirectoryDraft] = useState('');
  const [opening, setOpening] = useState(false);
  const [error, setError] = useState('');
  const [selection, setSelection] = useState<GridSelection | null>(null);
  const [portMarks, setPortMarks] = useState<PortMarks>({});
  const [hosts, setHosts] = useState<string[] | null>(null);
  const [search, setSearch] = useState('');
  const [filterRules, setFilterRules] = useState<FilterRule[]>([]);
  const [filterEdit, setFilterEdit] = useState<{ field: FilterAction; original: FilterRule[] } | null>(null);
  const [filterPosition, setFilterPosition] = useState<{ x: number; y: number } | null>(null);
  const include = filterRules.filter((r) => r.enabled && r.action === 'include' && r.text).map((r) => r.text).join(' | ');
  const exclude = filterRules.filter((r) => r.enabled && r.action === 'exclude' && r.text).map((r) => r.text).join(' | ');
  const [filterPrefixPending, setFilterPrefixPending] = useState(false);
  const filterPrefixTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [errorsOnly, setErrorsOnly] = useState(false);
  const [start, setStart] = useState('');
  const [end, setEnd] = useState('');
  const [jump, setJump] = useState('');
  const [raw, setRaw] = useState(false);
  const [fullErrors, setFullErrors] = useState(false);
  const [wrap, setWrap] = useState(false);
  const [numberMode, setNumberMode] = useState<'off' | 'source' | 'overall'>('off');
  function cycleNumberMode() { setNumberMode((mode) => mode === 'off' ? 'source' : mode === 'source' ? 'overall' : 'off'); }
  const [merged, setMerged] = useState(embedded);
  const [linked, setLinked] = useState(true);
  const [options, setOptions] = useState(false);
  const [archiveOpen, setArchiveOpen] = useState(false);
  const [archiveMessage, setArchiveMessage] = useState('');
  const [indexPaused, setIndexPaused] = useState(false);
  const [context, setContext] = useState(0);
  const [follow, setFollow] = useState(false);
  // Pausing follow keeps the displayed tail page, including quiet hosts.
  const [tailPage, setTailPage] = useState(false);
  const newest = follow || tailPage;
  const [live, setLive] = useState(false);
  const [nextRefreshAt, setNextRefreshAt] = useState<number | null>(null);
  const [job, setJob] = useState('');
  const [downloads, setDownloads] = useState<DownloadRequest[]>([]);
  const [activeDownload, setActiveDownload] = useState<number | null>(null);
  const downloadId = useRef(0);
  const downloadGeneration = useRef(0);
  const viewRef = useRef<LogView | null>(null);
  viewRef.current = view;
  const [progress, setProgress] = useState<LogProgress | null>(null);
  const [indexProgress, setIndexProgress] = useState<LogProgress | null>(null);
  const [page, setPage] = useState<LogPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [offset, setOffset] = useState(0);
  const [revision, setRevision] = useState(0);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [cursor, setCursor] = useState<LogRow | null>(null);
  const [lastCursorTime, setLastCursorTime] = useState<string | null>(null);
  const [lineMenu, setLineMenu] = useState<{ row: LogRow; text: string; x: number; y: number } | null>(null);
  const undoWindow = useRef<{ start: string; end: string } | null>(null);
  const pendingPage = useRef<'first' | 'last' | null>(null);
  const pendingTimeJump = useRef<{ target: string; host: string | null; sourcePage: LogPage | null } | null>(null);
  const sequence = useRef<{ bound: 't' | 'T' | null; digits: string; unit?: 'h'; time: string | null; original: { start: string; end: string }; timer: ReturnType<typeof setTimeout> } | null>(null);
  const sessionRef = useRef<number | null>(null);
  const loadedKey = useRef('');
  const jobRef = useRef(false);
  const panes = useRef<Record<string, HTMLDivElement | null>>({});
  const syncing = useRef(false);
  const queryRequest = useRef(0);
  const dispatchedText = useRef('');
  const selectionKey = `${connection.server}\0${connection.appServer ?? ''}\0${directory}`;

  useEffect(() => {
    let alive = true;
    setHealth('checking'); setServerTime('');
    if (!connection.server) { setHealth('disconnected'); return; }
    const check = () => {
      void logsApi.health(connection).then((time) => {
        if (alive) { setServerTime(time); setClock(Date.now()); setHealth('connected'); }
      }).catch(() => { if (alive) setHealth('disconnected'); });
    };
    check();
    const timer = setInterval(check, 300_000);
    return () => { alive = false; clearInterval(timer); };
  }, [connection.server, connection.appServer, connection.database]);
  useEffect(() => { const timer = setInterval(() => setClock(Date.now()), 60_000); return () => clearInterval(timer); }, []);

  function clearFilterPrefix() {
    if (filterPrefixTimer.current) clearTimeout(filterPrefixTimer.current);
    filterPrefixTimer.current = null; setFilterPrefixPending(false);
  }
  function openFilter(field: 'include' | 'exclude') {
    clearFilterPrefix(); setOptions(false); setLineMenu(null); setReading(true);
    setFilterEdit({ field, original: structuredClone(filterRules) });
  }
  function closeFilter(cancel = false) {
    if (cancel && filterEdit) { setFilterRules(filterEdit.original); setError(''); setOffset(0); setTailPage(false); }
    setFilterEdit(null);
    requestAnimationFrame(() => (panes.current[merged ? 'Merged timeline' : leadingHost] ?? Object.values(panes.current).find(Boolean))?.focus({ preventScroll: true }));
  }
  function clearFilters() {
    setFilterRules([]); setSearch(''); setOffset(0); setTailPage(false); setError('');
  }
  function pauseFollow() { if (follow) setTailPage(true); setFollow(false); }
  function toggleFollow() { if (follow) pauseFollow(); else setFollow(true); }
  useEffect(() => { if (!active || !reading || options) clearFilterPrefix(); }, [active, reading, options]);
  useEffect(() => () => { if (filterPrefixTimer.current) clearTimeout(filterPrefixTimer.current); }, []);

  useEffect(() => {
    if (!active || !connection.server || loadedKey.current === selectionKey) return;
    let alive = true;
    if (sessionRef.current !== null) void logsApi.close(sessionRef.current).catch(() => {});
    sessionRef.current = null;
    setOpening(true); setError(''); setLive(false); setArchiveOpen(false); setArchiveMessage(''); setIndexPaused(false); setView(null); setPage(null); setExpanded(null); setCursor(null); setHosts(null); setGridMode(embedded ? 'hidden' : 'expanded'); setReading(embedded); setMaximized(false); setFilterEdit(null); setLastCursorTime(null);
    setJob(''); setProgress(null); setDownloads([]); setActiveDownload(null); downloadGeneration.current++; jobRef.current = false;
    void (async () => {
      const opened = await logsApi.open(connection, directory);
      if (!alive) { void logsApi.close(opened.session).catch(() => {}); return; }
      sessionRef.current = opened.session;
      let next = opened;
      if (embedded) {
        try { next = await logsApi.discover(opened.session, value => { if (alive) setProgress(value); }); }
        catch (e) { if (alive) setError(errorMessage(e)); }
      }
      if (!alive) return;
      loadedKey.current = selectionKey;
      setView(next); setSelection(embedded ? {mode:'cell',port,kind:'ERR',day:currentServerDay(next.now,Date.now())} : defaultGridSelection(next.files, currentServerDay(next.now, Date.now()))); setPortMarks({}); setOffset(0); setTailPage(false); setFollow(embedded); setLive(embedded); setProgress(null);
    })().catch((e) => { if (alive) setError(errorMessage(e)); }).finally(() => { if (alive) setOpening(false); });
    return () => { alive = false; };
  }, [active, selectionKey, connection.server, connection.appServer, directory, embedded]);

  useEffect(() => () => { if (sessionRef.current !== null) void logsApi.close(sessionRef.current).catch(() => {}); }, []);

  const today = currentServerDay(serverTime || view?.now || '', clock);
  useEffect(() => {
    if (!embedded || !view) return;
    setSelection({mode:'cell', port, kind:'ERR', day:today});
    setMerged(true); setOffset(0); setTailPage(false); setFollow(true);
  }, [embedded, port, today, view?.session]);
  const effectiveSelection = useMemo<GridSelection | null>(() => merged && selection?.mode === 'cell'
    ? { ...selection, mode: 'cells', cells: [{ ...selection, kind: 'ERR' }, { ...selection, kind: 'ACC' }] } : selection, [merged, selection]);
  const scope = useMemo(() => selectedSources(view?.files ?? [], effectiveSelection, today, portMarks), [view, effectiveSelection, today, portMarks]);
  const selectedFiles = useMemo(() => scope.files.filter((file) => hosts === null || hosts.includes(file.host)), [scope, hosts]);
  const sourceKey = selectedFiles.filter((file) => file.cached).map((file) => file.id).sort().join(',');
  const scopeKey = JSON.stringify(scope.scopes.filter((cell) => selectedFiles.some((file) => file.id === cell.source && file.cached)));
  const hostKey = hosts?.slice().sort().join(',') ?? '';
  useEffect(() => {
    if (!active || !view || indexPaused) return;
    let alive = true;
    const session = view.session;
    const requestId = ++queryRequest.current;
    let dispatched = false;
    const textKey = JSON.stringify([search, filterRules]);
    const textChanged = dispatchedText.current !== textKey;
    setLoading(true); setIndexProgress(null);
    const timer = setTimeout(() => {
      dispatched = true; dispatchedText.current = textKey;
      void logsApi.page(session, {
        requestId, merged, splitHosts: !merged, cells: JSON.parse(scopeKey),
        files: sourceKey ? sourceKey.split(',') : [], hosts: hostKey ? hostKey.split(',') : [],
        search, include: '', exclude: '', filterRules, errorsOnly, start: normalizeTime(start), end: normalizeTime(end), offset, newest,
        raw, fullErrors, contextBefore: context, contextAfter: context,
      }, (value) => { if (alive) setIndexProgress(value); }).then((next) => {
        if (!alive) return;
        setPage(next);
        if (next.coverage) setView((old) => {
          if (!old || old.session !== session) return old;
          let changed = false;
          const files = old.files.map((file) => {
            const days = next.coverage?.[file.id];
            if (!days || JSON.stringify(file.days ?? []) === JSON.stringify(days)) return file;
            changed = true; return { ...file, days };
          });
          return changed ? { ...old, files } : old;
        });
      }).catch((e) => { if (alive) setError(errorMessage(e)); }).finally(() => { if (alive) { setLoading(false); setIndexProgress(null); } });
    }, textChanged ? 120 : 0);
    return () => { alive = false; clearTimeout(timer); if (dispatched) void logsApi.cancelSearch(session, requestId).catch(() => {}); };
  }, [active, view?.session, indexPaused, revision, sourceKey, scopeKey, merged, hostKey, search, filterRules, errorsOnly, start, end, offset, newest, raw, fullErrors, context]);

  async function saveArchive(settings: ArchiveSettings) {
    if (!view) return;
    const session = view.session;
    await logsApi.saveArchiveSettings(session, settings);
    if (sessionRef.current !== session) return;
    setView(old => old ? { ...old, archiveSettings: settings } : old);
    setArchiveMessage('Archive settings saved.');
  }
  async function runArchive(settings: ArchiveSettings) {
    if (!view || jobRef.current) return;
    const session = view.session;
    jobRef.current = true; setIndexPaused(true); setLoading(false); setIndexProgress(null); setJob('Archiving logs'); setLive(false); setFollow(false); setProgress(null); setArchiveMessage(''); setError('');
    try {
      const result = await logsApi.archive(session, settings, value => { if (sessionRef.current === session) setProgress(value); });
      if (sessionRef.current !== session) return;
      setView(result.view);
      const report = result.report;
      setArchiveMessage(`${report.compressed} segments compressed · ${report.archived} archived · ${formatBytes(report.retiredBytes)} of original files retired · ${formatBytes(report.indexBytesReclaimed)} of record index reclaimed.${report.warnings.length ? ` ${report.warnings.join(' ')}` : ''}`);
    } catch (e) { if (sessionRef.current === session) setError(errorMessage(e)); }
    finally { if (sessionRef.current === session) { setJob(''); jobRef.current = false; setProgress(null); } }
  }
  async function discover() {
    if (!view || jobRef.current) return;
    const session = view.session;
    jobRef.current = true; setJob('Discovering logs'); setError(''); setProgress(null);
    try {
      const next = await logsApi.discover(session, (value) => { if (sessionRef.current === session) setProgress(value); });
      if (sessionRef.current !== session) return;
      setView(next); setRevision((value) => value + 1);
      if (!selection) setSelection(defaultGridSelection(next.files, currentServerDay(next.now, Date.now())));
    } catch (e) { if (sessionRef.current === session) setError(errorMessage(e)); }
    finally { if (sessionRef.current === session) { setJob(''); jobRef.current = false; } }
  }
  async function runDownload(request: DownloadRequest, automatic: boolean) {
    if (jobRef.current || sessionRef.current !== request.session) return;
    const session = request.session;
    const generation = downloadGeneration.current;
    jobRef.current = true;
    if (!automatic) setActiveDownload(request.id);
    setJob(request.full ? 'Downloading full files' : 'Refreshing log tails'); setProgress(null);
    try {
      const latest = viewRef.current;
      let selected = latest?.session === session
        ? selectedSources(latest.files, request.selection, currentServerDay(latest.now, Date.now()), request.marks).files.filter((file) => request.hosts === null || request.hosts.includes(file.host))
        : request.files;
      let files = selected.filter((file) => file.remote && (!automatic || rotationOf(file.name) === 0));
      if (automatic && !files.length) return;
      if (!automatic && (!files.length || selected.some((file) => !file.remote))) {
        setJob('Discovering selected logs');
        const discovered = await logsApi.discover(session, (value) => { if (sessionRef.current === session) setProgress(value); });
        if (sessionRef.current !== session || generation !== downloadGeneration.current) return;
        viewRef.current = discovered; setView(discovered);
        selected = selectedSources(discovered.files, request.selection, currentServerDay(discovered.now, Date.now()), request.marks).files
          .filter((file) => request.hosts === null || request.hosts.includes(file.host));
        files = selected.filter((file) => file.remote);
        if (!files.length) {
          const failures = Object.entries(discovered.errors).map(([host, message]) => `${host}: ${message}`).join('\n');
          throw new Error(failures ? `Could not find selected remote logs.\n${failures}` : 'No remote files found for the selected port, log type and day.');
        }
      }
      if (generation !== downloadGeneration.current) return;
      if (!automatic) setDownloads((old) => old.map((item) => item.id === request.id ? { ...item, files } : item));
      setJob(request.full ? 'Downloading full files' : 'Refreshing log tails'); setProgress(null);
      const next = await logsApi.download(session, files.map((file) => file.id), request.full,
        (value) => { if (sessionRef.current === session) setProgress(value); }, automatic);
      if (sessionRef.current !== session || generation !== downloadGeneration.current) return;
      viewRef.current = next; setView(next); setRevision((value) => value + 1);
    } catch (e) { if (sessionRef.current === session && generation === downloadGeneration.current) { setError(errorMessage(e)); if (automatic) setLive(false); } }
    finally {
      if (sessionRef.current === session) {
        if (!automatic) { setDownloads((old) => old.filter((item) => item.id !== request.id)); setActiveDownload(null); }
        setJob(''); setProgress(null); jobRef.current = false;
      }
    }
  }
  function download(full: boolean, automatic = false) {
    if (!view || !effectiveSelection || hosts?.length === 0) return;
    const request: DownloadRequest = { id: ++downloadId.current, session: view.session, selection: effectiveSelection,
      marks: { ...portMarks }, hosts: hosts && [...hosts], files: selectedFiles, full };
    if (automatic) { if (!jobRef.current && !downloads.length) void runDownload(request, true); }
    else { setError(''); setDownloads((old) => [...old, request]); }
  }
  const downloadRunner = useRef(runDownload); downloadRunner.current = runDownload;
  useEffect(() => {
    if (!downloads.length || jobRef.current || sessionRef.current !== downloads[0].session) return;
    void downloadRunner.current(downloads[0], false);
  }, [downloads, job, view?.session]);
  function cancelDownloads() {
    downloadGeneration.current++;
    setDownloads([]);
    if (view && jobRef.current) void logsApi.cancel(view.session).catch((e) => setError(errorMessage(e)));
  }
  const liveTick = useRef(download); liveTick.current = download;
  useEffect(() => {
    if (!active || !live) { setNextRefreshAt(null); return; }
    void liveTick.current(false, true);
    setNextRefreshAt(Date.now() + refreshInterval);
    const timer = setInterval(() => { void liveTick.current(false, true); setNextRefreshAt(Date.now() + refreshInterval); }, refreshInterval);
    return () => clearInterval(timer);
  }, [active, live, view?.session, refreshInterval]);

  useEffect(() => {
    if (!follow || !page) return;
    syncing.current = true;
    const frame = requestAnimationFrame(() => {
      for (const pane of Object.values(panes.current)) if (pane) pane.scrollTop = pane.scrollHeight;
      syncing.current = false;
    });
    return () => { cancelAnimationFrame(frame); syncing.current = false; };
  }, [follow, page]);

  const selectedRemote = selectedFiles.filter((file) => file.remote);
  const hostColors = useMemo(() => new Map((view?.hosts ?? []).map((host, index) => [host,
    HOST_COLORS[index] ?? `hsl(${(index * 137.508 + 195) % 360} 78% 76%)`])), [view?.hosts]);
  const mergedPositions = useMemo(() => new Map((page?.rows ?? []).map((row, index) => [row.id, index])), [page]);
  const selectedHosts = hosts ?? view?.hosts ?? [];
  const shownHosts = (page?.hosts.map((item) => item.host) ?? []).filter((host) => selectedHosts.includes(host));
  const visibleRows = useMemo(() => (page?.rows ?? []).filter((row) => (!start && !end) ||
    (row.time && (!start || row.time >= normalizeTime(start)!) && (!end || row.time <= normalizeTime(end)!))), [page, start, end]);
  useEffect(() => { if (cursor?.time) setLastCursorTime(cursor.time); }, [cursor?.time]);
  const selectedTime = cursor?.time && visibleRows.some((row) => row.id === cursor.id) ? cursor.time : null;
  const oldTimeInWindow = lastCursorTime && (!start || lastCursorTime >= normalizeTime(start)!) && (!end || lastCursorTime <= normalizeTime(end)!) ? lastCursorTime : null;
  const referenceTime = selectedTime ?? oldTimeInWindow ?? (start && end ? midpointTime(start, end) : start || end || null);

  useEffect(() => {
    if (!page) return;
    if (!visibleRows.length) { if (!loading) { pendingPage.current = null; setCursor(null); } return; }
    const host = shownHosts.includes(leadingHost) ? leadingHost : shownHosts[0];
    const rows = merged ? visibleRows : visibleRows.filter((row) => row.host === host);
    if (!rows.length) return;
    const jump = pendingTimeJump.current;
    if (jump) {
      if (page === jump.sourcePage) return;
      const candidates = merged ? rows : rows.filter((row) => row.host === jump.host);
      const target = Date.parse(`${jump.target}Z`);
      const closest = candidates.filter((row) => row.time).reduce<LogRow | null>((best, row) =>
        !best || Math.abs(Date.parse(`${row.time}Z`) - target) < Math.abs(Date.parse(`${best.time}Z`) - target) ? row : best, null);
      if (closest) { pendingTimeJump.current = null; setCursor(closest); return; }
    }
    const edge = pendingPage.current;
    pendingPage.current = null;
    setCursor((old) => edge ? rows[edge === 'first' ? 0 : rows.length - 1] :
      rows.find((row) => row.id === old?.id) ?? rows.find((row) => old?.time && row.time && row.time >= old.time) ?? rows[0]);
  }, [page, visibleRows, leadingHost, merged]);
  useEffect(() => {
    if (page && !loading && !visibleRows.length) { pendingPage.current = null; setCursor(null); }
  }, [page, loading, visibleRows.length]);
  useEffect(() => {
    if (!cursor || !reading || expanded) return;
    const pane = panes.current[merged ? 'Merged timeline' : cursor.host];
    const row = [...(pane?.querySelectorAll<HTMLElement>('[data-record-id]') ?? [])].find((element) => element.dataset.recordId === cursor.id);
    row?.scrollIntoView?.({ block: 'nearest' });
    if (pane && linked) syncScroll(merged ? 'Merged timeline' : cursor.host, pane);
  }, [cursor, page, reading, merged, expanded]);
  useEffect(() => () => { if (sequence.current) clearTimeout(sequence.current.timer); }, []);
  useEffect(() => {
    if (!lineMenu) return;
    const close = () => setLineMenu(null);
    window.addEventListener('click', close);
    return () => window.removeEventListener('click', close);
  }, [lineMenu]);

  const selectRecord = useCallback((row: LogRow) => { setCursor(row); setLeadingHost(row.host); setReading(true); }, []);
  const expandRecord = useCallback((id: string) => {
    if (expanded !== id && newest) { setFollow(false); setTailPage(false); setOffset(page?.offset ?? 0); }
    setExpanded((old) => old === id ? null : id);
  }, [expanded, newest, page?.offset]);
  const openLineMenu = useCallback((event: React.MouseEvent, row: LogRow) => {
    event.preventDefault(); event.stopPropagation(); setCursor(row); setLeadingHost(row.host); setReading(true);
    const selection = window.getSelection();
    const text = selection && !selection.isCollapsed && selection.rangeCount && event.currentTarget.contains(selection.anchorNode) && event.currentTarget.contains(selection.focusNode) ? selection.toString() : '';
    setLineMenu({row,text,x:Math.max(8,Math.min(event.clientX,window.innerWidth-308)),y:Math.max(8,Math.min(event.clientY,window.innerHeight-620))});
  }, []);
  function changeRules(next: FilterRule[]) {
    setFilterRules(next); setOffset(0); setTailPage(false); setError('');
  }
  function filterFromLog(action: FilterAction, text: string, field: FilterField = 'any') {
    changeRules(appendRules(filterRules, [newRule(text, action, { field, mode: field === 'any' ? 'literal' : 'exact', case: 'sensitive' })]));
    setLineMenu(null);
  }
  const chooseGrid = useCallback((next: GridSelection) => {
    setIndexPaused(false); setSelection(next); setOffset(0); setTailPage(false); setFollow(false); setError('');
    setMerged(next.mode !== 'cell' && next.mode !== 'cells');
  }, []);
  const focusLogs = useCallback(() => setReading(true), []);
  const openLogs = useCallback(() => {
    setIndexPaused(false); focusLogs();
    requestAnimationFrame(() => Object.values(panes.current).find((pane) => pane !== null)?.focus());
  }, [focusLogs]);
  function toggleMerge() {
    if (selection) setSelection({ ...selection, mode: 'cell', kind: selection.kind === 'REQ' ? 'ERR' : selection.kind });
    setMerged((value) => !value); setOffset(0); setTailPage(false); setMaximized(false); focusLogs();
  }
  const applyWindow = useCallback((nextStart: string, nextEnd: string, remember = true) => {
    if (nextStart && nextEnd && nextStart > nextEnd) { setError('Start must not be after end.'); return; }
    if (remember && (start !== nextStart || end !== nextEnd)) undoWindow.current = { start, end };
    setStart(nextStart); setEnd(nextEnd); setOffset(0); setTailPage(false); setFollow(false); setError('');
  }, [start, end]);
  function extendWindow(side: 'before' | 'after', minutes: number | 'all') {
    const existing = side === 'before' ? start : end;
    if (minutes === 'all' && !existing) return;
    const bound = existing || referenceTime;
    if (minutes !== 'all' && !bound) return;
    const next = minutes === 'all' ? '' : shiftTime(bound!, (side === 'before' ? -1 : 1) * minutes * 60_000);
    if (next === null) { setError('Window adjustment is out of range.'); return; }
    applyWindow(side === 'before' ? next : start, side === 'after' ? next : end);
  }
  function applySequence(bound: 't' | 'T' | null, digits: string, time: string | null) {
    if (!bound) { setError('Enter t or T after the number to adjust a window bound.'); return; }
    const current = bound === 't' ? start : end;
    const base = digits ? current || time : time;
    if (!base) { setError('Select a timestamped log entry first.'); return; }
    const next = digits ? shiftTime(base, Number(digits) * 60_000) : base;
    if (!next) { setError('Window adjustment is out of range.'); return; }
    applyWindow(bound === 't' ? next : start, bound === 'T' ? next : end, !digits);
  }
  function moveByTime(amount: number, direction: -1 | 1, unit: 'm' | 'h') {
    const base = cursor?.time ?? referenceTime ?? serverTime;
    if (!base) { setError('Select a timestamped log entry first.'); return; }
    let target = shiftTime(base, direction * amount * (unit === 'h' ? 3_600_000 : 60_000));
    if (!target) { setError('Select a timestamped log entry first.'); return; }
    const host = shownHosts.includes(leadingHost) ? leadingHost : shownHosts[0];
    const hostBounds = !merged && host ? page?.availableHosts?.[host] : undefined;
    const availableEdge = direction > 0 ? (hostBounds?.last ?? page?.availableLast ?? page?.last) : (hostBounds?.first ?? page?.availableFirst ?? page?.first);
    const hitLogEdge = Boolean(availableEdge && (direction > 0 ? target > availableEdge : target < availableEdge));
    if (hitLogEdge && availableEdge) target = availableEdge;
    const targetMs = Date.parse(`${target}Z`);
    const rows = merged ? visibleRows : visibleRows.filter((row) => row.host === host);
    const timedRows = rows.filter((row) => row.time);
    const pageStart = timedRows[0]?.time, pageEnd = timedRows[timedRows.length - 1]?.time;
    if (hitLogEdge || (start && target < normalizeTime(start)!) || (end && target > normalizeTime(end)!) || !pageStart || !pageEnd || target < pageStart || target > pageEnd) {
      const span = start && end ? Math.max(60_000, Date.parse(`${normalizeTime(end)}Z`) - Date.parse(`${normalizeTime(start)}Z`)) : 600_000;
      let left = targetMs - span / 2, right = targetMs + span / 2;
      const availableFirst = hostBounds?.first ?? page?.availableFirst;
      const availableLast = hostBounds?.last ?? page?.availableLast;
      if (availableFirst) left = Math.max(left, Date.parse(`${availableFirst}Z`));
      if (availableLast) right = Math.min(right, Date.parse(`${availableLast}Z`));
      pendingTimeJump.current = { target, host: host ?? null, sourcePage: page };
      applyWindow(new Date(left).toISOString().slice(0, 23), new Date(right).toISOString().slice(0, 23));
      return;
    }
    const next = rows.reduce<LogRow | null>((best, row) => {
      if (!row.time || (direction < 0 ? row.time > base : row.time < base)) return best;
      if (!best || Math.abs(Date.parse(`${row.time}Z`) - targetMs) < Math.abs(Date.parse(`${best.time}Z`) - targetMs)) return row;
      return best;
    }, null);
    if (next) { pauseFollow(); setCursor(next); setLeadingHost(next.host); }
  }
  function timeNavigationKey(key: string) {
    const current = sequence.current;
    if (current && !current.bound && current.digits && key === 'h') {
      clearTimeout(current.timer);
      const next = { ...current, unit: 'h' as const };
      next.timer = setTimeout(() => { sequence.current = null; }, 2500);
      sequence.current = next;
      return true;
    }
    if (current && !current.bound && current.digits && (key === 'd' || key === 'u')) {
      clearTimeout(current.timer); sequence.current = null;
      moveByTime(Number(current.digits), key === 'd' ? 1 : -1, current.unit ?? 'm');
      return true;
    }
    return false;
  }
  function truncationKey(key: string) {
    const digit = /^\d$/.test(key), bound = key === 't' || key === 'T' ? key : null;
    if (!digit && !bound) return false;
    const prior = sequence.current;
    if (prior) clearTimeout(prior.timer);
    if (prior && bound && prior.bound === 't' && bound === 't' && !prior.digits) {
      sequence.current = null;
      if (!prior.time) { setError('Select a timestamped log entry first.'); return true; }
      const left = shiftTime(prior.time, -150_000), right = shiftTime(prior.time, 150_000);
      if (left && right) { undoWindow.current = prior.original; applyWindow(left, right, false); }
      return true;
    }
    if (prior && bound && prior.bound === null) {
      sequence.current = null;
      const base = (bound === 't' ? start : end) || prior.time;
      const next = base && shiftTime(base, -Number(prior.digits) * 60_000);
      if (next) applyWindow(bound === 't' ? next : start, bound === 'T' ? next : end);
      else setError('Select a timestamped log entry first.');
      return true;
    }
    if (prior && bound) { if (prior.digits && prior.bound) undoWindow.current = prior.original; applySequence(prior.bound, prior.digits, prior.time); }
    const next = { bound: digit ? prior?.bound ?? null : bound, digits: digit ? (prior?.digits ?? '') + key : '', time: prior?.time ?? cursor?.time ?? null, original: prior?.original ?? { start, end } };
    if (bound && cursor?.time) applyWindow(bound === 't' ? cursor.time : start, bound === 'T' ? cursor.time : end);
    const timer = setTimeout(() => { sequence.current = null; if (next.digits && next.bound) undoWindow.current = next.original; if (next.digits && next.bound) keyActions.current.finishSequence(next.bound, next.digits, next.time); }, next.bound ? 700 : 2500);
    sequence.current = { ...next, timer };
    return true;
  }
  function moveCursor(delta: number) {
    if (!page) return;
    const host = shownHosts.includes(leadingHost) ? leadingHost : shownHosts[0];
    const rows = merged ? visibleRows : visibleRows.filter((row) => row.host === host);
    if (!rows.length) return;
    const index = rows.findIndex((row) => row.id === cursor?.id);
    const next = (index < 0 ? 0 : index) + delta;
    const advance = merged ? page.rows.length : (page.pageSize ?? 200);
    if (!loading && next < 0 && page.offset > 0) { pendingPage.current = 'last'; setFollow(false); setTailPage(false); setOffset(Math.max(0, page.offset - (page.pageSize ?? 200))); }
    else if (!loading && next >= rows.length && page.offset + advance < (page.pageTotal ?? page.total)) { pendingPage.current = 'first'; setFollow(false); setTailPage(false); setOffset(page.offset + advance); }
    else { pauseFollow(); setCursor(rows[Math.max(0, Math.min(rows.length - 1, next))]); }
  }
  function contentKey(key: string) {
    if (filterPrefixTimer.current) {
      clearFilterPrefix();
      if (key === 'i' || key === 'o') { openFilter(key === 'i' ? 'include' : 'exclude'); return true; }
      if (key === 'c') { clearFilters(); return true; }
      toggleFollow();
    }
    if (timeNavigationKey(key)) return true;
    if (truncationKey(key)) return true;
    if (sequence.current) { const old = sequence.current; clearTimeout(old.timer); sequence.current = null; if (old.unit) return false; if (old.digits && old.bound) undoWindow.current = old.original; applySequence(old.bound, old.digits, old.time); }
    if (key === 'f') {
      setFilterPrefixPending(true);
      filterPrefixTimer.current = setTimeout(() => {
        filterPrefixTimer.current = null; setFilterPrefixPending(false); keyActions.current.toggleFollow();
      }, 700);
      return true;
    }
    if (key === 'j' || key === 'ArrowDown') moveCursor(1);
    else if (key === 'k' || key === 'ArrowUp') moveCursor(-1);
    else if (key === 'u' || key === 'PageUp' || key === 'd' || key === 'PageDown' || key === ' ') {
      const pane = panes.current[merged ? 'Merged timeline' : leadingHost || shownHosts[0]];
      const amount = Math.max(1, Math.floor((pane?.clientHeight || 170) / 17));
      moveCursor((key === 'u' || key === 'PageUp' ? -1 : 1) * amount);
    }
    else if (key === 'g' || key === 'Home' || key === 'G' || key === 'End') {
      if (!page) return true;
      const last = key === 'G' || key === 'End';
      const host = shownHosts.includes(leadingHost) ? leadingHost : shownHosts[0];
      const rows = merged ? visibleRows : visibleRows.filter((row) => row.host === host);
      const hostCount = page.hosts.find((item) => item.host === host)?.count ?? 0;
      const atEdge = !loading && (last ? newest || (merged ? page.offset + page.rows.length >= page.total : page.offset === 0 && rows.length >= hostCount) : !newest && page.offset === 0);
      if (atEdge && rows.length) { pauseFollow(); setCursor(rows[last ? rows.length - 1 : 0]); }
      else {
        pendingPage.current = last ? 'last' : 'first';
        setFollow(false); setTailPage(last); if (!last) setOffset(0);
      }
    }
    else if (key === 'h' || key === 'ArrowLeft' || key === 'l' || key === 'ArrowRight') {
      const pane = panes.current[merged ? 'Merged timeline' : leadingHost || shownHosts[0]];
      if (pane) pane.scrollLeft += key === 'h' || key === 'ArrowLeft' ? -80 : 80;
    }
    else if ((key === 'H' || key === 'L') && !merged && shownHosts.length) {
      const index = Math.max(0, shownHosts.indexOf(leadingHost));
      setLeadingHost(shownHosts[(index + (key === 'H' ? -1 : 1) + shownHosts.length) % shownHosts.length]);
    }
    else if (key === 'v') { if (merged) setOptions(true); else if (cursor) expandRecord(cursor.id); }
    else if (key === '#') cycleNumberMode();
    else if (key === 'U') { const previous = undoWindow.current; if (previous) { undoWindow.current = null; applyWindow(previous.start, previous.end, false); } }
    else return false;
    return true;
  }
  const keyActions = useRef({ toggleMerge, toggleFollow, gridMode, reading, download, contentKey, finishSequence: applySequence, options, lineMenu, filterEdit, closeFilter }); keyActions.current = { toggleMerge, toggleFollow, gridMode, reading, download, contentKey, finishSequence: applySequence, options, lineMenu, filterEdit, closeFilter };
  useEffect(() => {
    if (!active) return;
    function key(event: globalThis.KeyboardEvent) {
      const target = event.target;
      if (embedded && (!(target instanceof Node) || !workspaceElement.current?.contains(target))) return;
      if (target instanceof HTMLElement && (target.matches('input,textarea,select') || target.isContentEditable)) return;
      if (event.key === 'Escape' && keyActions.current.lineMenu) { event.preventDefault(); setLineMenu(null); return; }
      if (event.ctrlKey && !event.metaKey && !event.altKey && event.key.toLowerCase() === 't') { event.preventDefault(); setOptions(true); return; }
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      if (keyActions.current.filterEdit) { if (event.key === 'Escape') { event.preventDefault(); keyActions.current.closeFilter(true); } return; }
      if (keyActions.current.options) { if (event.key === 'Escape' || event.key === 'v') { event.preventDefault(); setOptions(false); } return; }
      const inGrid = target instanceof HTMLElement && Boolean(target.closest('.log-grid-panel'));
      if (!inGrid && keyActions.current.reading && keyActions.current.contentKey(event.key)) { event.preventDefault(); return; }
      if (event.key === '\\') { event.preventDefault(); setReading(true); setGridMode((mode) => mode === 'hidden' ? 'expanded' : 'hidden'); }
      else if (event.key === 'm') { event.preventDefault(); keyActions.current.toggleMerge(); }
      else if (event.key === 'd' && inGrid) { event.preventDefault(); void keyActions.current.download(true); }
      else if (event.key === 'z') { event.preventDefault(); setMaximized((value) => !value); }
      else if (event.key === 'Escape' && keyActions.current.reading) { setReading(false); }
    }
    window.addEventListener('keydown',key); return () => window.removeEventListener('keydown',key);
  }, [active, embedded]);
  function changeFilter(setter: (value: string) => void, value: string) { setter(value); setOffset(0); setTailPage(false); setError(''); }
  const selectTime = useCallback((first: number, last: number) => {
    if (!page?.timeline.length) return;
    const a = Math.min(first, last), b = Math.max(first, last);
    applyWindow(page.timeline[a].time, page.timeline[b + 1]?.time ?? page.last ?? page.timeline[b].time);
  }, [page, applyWindow]);
  function syncScroll(host: string, element: HTMLDivElement) {
    if (syncing.current) return;
    if (follow && element.scrollHeight - element.clientHeight - element.scrollTop > 80) pauseFollow();
    if (!linked) return;
    const top = element.getBoundingClientRect().top;
    const row = [...element.querySelectorAll<HTMLElement>('[data-time]')].find((row) => row.getBoundingClientRect().bottom > top + 8);
    const time = row?.dataset.time;
    if (!time) return;
    syncing.current = true;
    for (const [other, pane] of Object.entries(panes.current)) {
      if (other === host || !pane) continue;
      const target = [...pane.querySelectorAll<HTMLElement>('[data-time]')].find((row) => (row.dataset.time ?? '') >= time);
      if (target) pane.scrollTop += target.getBoundingClientRect().top - pane.getBoundingClientRect().top;
    }
    requestAnimationFrame(() => { syncing.current = false; });
  }

  const gridTransfers = useMemo(() => downloads.map((item) => ({ selection:item.selection, marks:item.marks, files:item.files, active:item.id===activeDownload, progress:item.id===activeDownload?progress:null })), [downloads, activeDownload, progress]);
  const activateGrid = useCallback(() => setReading(false), []);
  const markPorts = useCallback((marks: PortMarks) => { setPortMarks(marks); setOffset(0); setTailPage(false); }, []);
  const pageSize = page?.pageSize ?? 200;
  const pageTotal = page?.pageTotal ?? page?.total ?? 0;
  const pageAdvance = merged && page?.rows.length ? page.rows.length : pageSize;
  // A page edge is not necessarily an edge of the selected time window.
  const hasEarlierRecords = Boolean(page && start && page.availableFirst && page.availableFirst < normalizeTime(start)! && page.offset === 0);
  const hasLaterRecords = Boolean(page && end && page.availableLast && page.availableLast > normalizeTime(end)! && (page.total === 0 || page.offset + pageAdvance >= pageTotal));
  const referenceHost = shownHosts.includes(leadingHost) ? leadingHost : shownHosts[0];
  const hasContentFilter = Boolean(search.trim() || include.trim() || exclude.trim() || errorsOnly);
  const visibleHosts = merged ? ['Merged timeline'] : maximized ? [referenceHost].filter(Boolean) : shownHosts;
  useEffect(() => {
    const observers: IntersectionObserver[] = [];
    for (const pane of Object.values(panes.current)) {
      if (!pane) continue;
      const rows = pane.querySelectorAll<HTMLElement>('[data-record-id]');
      const first = rows[0], last = rows[rows.length - 1];
      if (!first || !last) continue;
      if (typeof IntersectionObserver === 'undefined') {
        pane.querySelectorAll('.window-edge-row').forEach((bar) => bar.classList.add('boundary-visible'));
        continue;
      }
      const observer = new IntersectionObserver((entries) => {
        for (const entry of entries) {
          if (entry.target === first) pane.querySelector('.window-edge-row[aria-label="Earlier times"]')?.classList.toggle('boundary-visible', entry.isIntersecting);
          if (entry.target === last) pane.querySelector('.window-edge-row[aria-label="Later times"]')?.classList.toggle('boundary-visible', entry.isIntersecting);
        }
      }, { root: pane, threshold: 0 });
      observer.observe(first);
      if (last !== first) observer.observe(last);
      observers.push(observer);
    }
    return () => observers.forEach((observer) => observer.disconnect());
  }, [page, hasEarlierRecords, hasLaterRecords, merged, maximized, visibleRows]);
  const mergeLabel = merged && selection?.mode === 'cell'
    ? `MERGED ${portLabel(selection.port)} · ${selection.day} · error + access` : `${merged ? 'MERGED · ' : ''}${scopeLabel(selection)}`;
  return <div ref={workspaceElement} className={`logs-workspace ${embedded ? 'logs-embedded' : ''}`} hidden={!active}>
    {lineMenu && <div className="line-context-menu" role="menu" aria-label="Log line actions" style={{left: Math.max(0,lineMenu.x), top: Math.max(0,lineMenu.y), maxHeight: `calc(100vh - ${Math.max(0,lineMenu.y) + 8}px)`}} onClick={(event)=>event.stopPropagation()}>
      <div className="line-context-title">{lineMenu.row.time?.replace('T',' ') ?? 'Untimed entry'}</div>
      {lineMenu.text && <>
        <div className="line-context-title" title={lineMenu.text}>Selected: {lineMenu.text.slice(0, 80)}</div>
        <button role="menuitem" onClick={() => filterFromLog('include', lineMenu.text)}>Include selected text</button>
        <button role="menuitem" onClick={() => filterFromLog('exclude', lineMenu.text)}>Exclude selected text</button>
      </>}
      {([['port', lineMenu.row.port], ['method', lineMenu.row.method], ['host', lineMenu.row.host], ['level', lineMenu.row.level], ['kind', lineMenu.row.kind]] as const).filter(([, value]) => value).map(([field, value]) => <div className="line-context-field" key={field}>
        <span>{field}: {value}</span><button role="menuitem" aria-label={`Keep ${field} ${value}`} onClick={() => filterFromLog('include', value!, field)}>Keep</button><button role="menuitem" aria-label={`Drop ${field} ${value}`} onClick={() => filterFromLog('exclude', value!, field)}>Drop</button>
      </div>)}
      <button role="menuitem" disabled={!lineMenu.row.time} onClick={()=>{applyWindow(lineMenu.row.time ?? '',end);setLineMenu(null);}}>Hide before this line <kbd>t</kbd></button>
      <button role="menuitem" disabled={!lineMenu.row.time} onClick={()=>{applyWindow(start,lineMenu.row.time ?? '');setLineMenu(null);}}>Hide after this line <kbd>T</kbd></button>
      <button role="menuitem" disabled={!lineMenu.row.time} onClick={()=>{const time=lineMenu.row.time; if(time){const a=shiftTime(time,-150000),b=shiftTime(time,150000);if(a&&b)applyWindow(a,b);}setLineMenu(null);}}>Show 5 minutes around line <kbd>tt</kbd></button>
      <button role="menuitem" disabled={!start&&!lineMenu.row.time} onClick={()=>{const time=start||lineMenu.row.time;const next=time&&shiftTime(time,-300000);if(next)applyWindow(next,end);setLineMenu(null);}}>Move start 5 minutes earlier <kbd>5t</kbd></button>
      <button role="menuitem" disabled={!start&&!lineMenu.row.time} onClick={()=>{const time=start||lineMenu.row.time;const next=time&&shiftTime(time,300000);if(next)applyWindow(next,end);setLineMenu(null);}}>Move start 5 minutes later <kbd>t5</kbd></button>
      <button role="menuitem" disabled={!end&&!lineMenu.row.time} onClick={()=>{const time=end||lineMenu.row.time;const next=time&&shiftTime(time,-300000);if(next)applyWindow(start,next);setLineMenu(null);}}>Move end 5 minutes earlier <kbd>5T</kbd></button>
      <button role="menuitem" disabled={!end&&!lineMenu.row.time} onClick={()=>{const time=end||lineMenu.row.time;const next=time&&shiftTime(time,300000);if(next)applyWindow(start,next);setLineMenu(null);}}>Move end 5 minutes later <kbd>T5</kbd></button>
      <button role="menuitem" disabled={!start&&!end} onClick={()=>{applyWindow('','');setLineMenu(null);}}>Clear time window</button>
      <button role="menuitem" disabled={!undoWindow.current} onClick={()=>{const old=undoWindow.current;if(old){undoWindow.current=null;applyWindow(old.start,old.end,false);}setLineMenu(null);}}>Undo window change <kbd>U</kbd></button>
      <button role="menuitem" onClick={()=>{if (expanded !== lineMenu.row.id) expandRecord(lineMenu.row.id);setLineMenu(null);}}>Show entry details <kbd>v</kbd></button>
    </div>}
    <div className="logs-toolbar">
      {embedded && <label className="query-log-port">Port <select aria-label="Query log port" value={port} onChange={event=>setPort(event.target.value)}>{[...new Set([port,...(view?.files.map(file=>file.port)??[])])].sort().map(value=><option key={value} value={value}>{portLabel(value)}</option>)}</select></label>}
      <span className={`connection-health ${health}`} role="status" aria-label={`Server connection: ${health}`} title={`Server connection: ${health} · checked every 5 minutes`}><span aria-hidden="true" />{health === 'connected' ? 'Connected' : health === 'disconnected' ? 'Disconnected' : 'Checking…'}</span>
      <button className="log-tool" aria-label={gridMode === 'hidden' ? 'Show files' : 'Hide files'} title="Show/hide files (\\)" aria-pressed={gridMode !== 'hidden'} onClick={() => {setReading(true);setGridMode((mode) => mode === 'hidden' ? 'expanded' : 'hidden');}}>Files</button>
      <button className="log-tool" aria-label="Browse files" aria-pressed={!reading && gridMode === 'expanded'} onClick={() => { setGridMode('expanded'); setReading(false); }}>Browse</button>
      <button className="log-tool" disabled={!view || Boolean(job) || opening} onClick={() => void discover()} title="Refresh host and file inventory">{job === 'Discovering logs' ? <LoaderCircle className="spin" size={12} /> : <RefreshCw size={12} />} Discover</button>
      <label className="compact-log-search"><Search size={12} /><input aria-label="Search log content" placeholder="Search /regex/i" value={search} onChange={(e) => changeFilter(setSearch, e.target.value)} /></label>
      <button className="log-tool" aria-label={merged ? 'Unmerge logs' : 'Merge error and access logs'} aria-pressed={merged} title="Merge current port's error + access logs (m)" onClick={toggleMerge}>{merged ? 'Unmerge' : 'Merge'}</button>
      <button className="log-tool download-tool" aria-label="Download selected logs" disabled={!view || !selection || !selectedHosts.length} onClick={() => void download(true)} title="Download the selected log files from enabled hosts (d)"><ArrowDownToLine size={12} /> Download <kbd>d</kbd></button>
      <button className="log-tool" disabled={!view || !selection || !selectedHosts.length} onClick={() => void download(false)} title="Refresh the tails of selected logs"><RefreshCw size={12} /> Tail</button>
      <label className="compact-toggle"><input type="checkbox" checked={live} disabled={!selectedRemote.some((file) => rotationOf(file.name) === 0)} onChange={(e) => { setLive(e.target.checked); if (e.target.checked) setFollow(true); }} />Live{embedded ? ` (${refreshInterval / 1000}s)` : ''}</label>{active && live && selectedRemote.some(file => rotationOf(file.name) === 0) && nextRefreshAt !== null && <RefreshCountdown next={nextRefreshAt} interval={refreshInterval} refreshing={job === 'Refreshing log tails'} />}
      <label className="compact-toggle"><input type="checkbox" checked={follow} onChange={(e) => { if (e.target.checked) setFollow(true); else pauseFollow(); }} />Follow</label>
      <button className="log-tool" disabled={!view || opening} aria-expanded={archiveOpen} onClick={() => { setArchiveOpen(!archiveOpen); setOptions(false); }}>Archive</button>
      <button className="log-tool" aria-label="Options" aria-expanded={options} title="Filters, display and time window" onClick={() => { setOptions(!options); setArchiveOpen(false); }}><SlidersHorizontal size={13} /></button>
    </div>
    {archiveOpen && view && <LogArchivePanel key={view.session} initial={view.archiveSettings ?? defaultArchiveSettings} busy={Boolean(job)} message={archiveMessage} onSave={saveArchive} onRun={runArchive} onClose={() => setArchiveOpen(false)} />}
    {options && <section className="log-options-popover" aria-label="Log options">
      <div className="options-title"><strong>Filters and display</strong><button aria-label="Close log options" onClick={() => setOptions(false)}><X size={14} /></button></div>
      <div className="log-option-fields"><button className="log-tool" onClick={() => openFilter('include')}>Manage keep terms ({filterRules.filter((r) => r.action === 'include').length})</button><button className="log-tool" onClick={() => openFilter('exclude')}>Manage drop terms ({filterRules.filter((r) => r.action === 'exclude').length})</button></div>
      <div className="log-option-checks"><label><input type="checkbox" checked={errorsOnly} onChange={(e) => { setErrorsOnly(e.target.checked); setOffset(0); setTailPage(false); }} />Errors only</label><label><input type="checkbox" checked={raw} onChange={(e) => setRaw(e.target.checked)} />Raw records</label><label><input type="checkbox" checked={fullErrors} onChange={(e) => setFullErrors(e.target.checked)} />Full XML errors</label><label>Line numbers (#) <select aria-label="Line numbers" value={numberMode} onChange={(e) => setNumberMode(e.target.value as 'off' | 'source' | 'overall')}><option value="off">Off</option><option value="source">File</option><option value="overall">Overall</option></select></label><label><input type="checkbox" checked={wrap} onChange={(e) => setWrap(e.target.checked)} />Wrap lines</label><label><input type="checkbox" checked={linked} onChange={(e) => setLinked(e.target.checked)} />Link host scrolling</label><label>Context <select aria-label="Context records" value={context} onChange={(e) => { setContext(Number(e.target.value)); setOffset(0); setTailPage(false); }}>{[0,1,2,5,10].map((value) => <option key={value} value={value}>{value} before / after</option>)}</select></label></div>
      <div className="log-time-controls"><label>From<input aria-label="Log start time" type="datetime-local" step="0.001" value={start} onChange={(e) => applyWindow(e.target.value, end)} /></label><label>To<input aria-label="Log end time" type="datetime-local" step="0.001" value={end} onChange={(e) => applyWindow(start, e.target.value)} /></label>
        <form onSubmit={(e) => { e.preventDefault(); applyWindow(jump, ''); }}><label>Jump<input aria-label="Jump to log time" type="datetime-local" step="0.001" value={jump} onChange={(e) => setJump(e.target.value)} /></label><button className="log-tool" disabled={!jump}>Go</button></form><button className="log-tool" disabled={!start && !end} onClick={() => applyWindow('', '')}>Reset time</button></div>
      <form className="log-directory" onSubmit={(e) => { e.preventDefault(); setDirectory(directoryDraft.trim()); }}><label>Remote log directory<input aria-label="Remote log directory" placeholder="MarkLogic default" value={directoryDraft} onChange={(e) => setDirectoryDraft(e.target.value)} /></label><button className="log-tool" disabled={Boolean(job)}>Apply directory</button></form>
    </section>}
    {indexPaused && !job && <div className="log-archive-notice" role="status">Reading paused after archival. Select a day to read its logs.<button className="log-tool" onClick={() => setIndexPaused(false)}>Read selected logs</button></div>}
    {error && <div className="compact-log-error" role="alert"><pre>{error}</pre><button aria-label="Dismiss log error" onClick={() => setError('')}><X size={12} /></button></div>}
    {view && Object.keys(view.errors).length > 0 && <details className="log-errors"><summary>{Object.keys(view.errors).length} host errors · cached logs available</summary>{Object.entries(view.errors).map(([host,message]) => <pre key={host}>{host}: {message}</pre>)}</details>}
    <div className={`log-work-area grid-${gridMode}`} data-testid="log-work-area">
      {gridMode !== 'hidden' && <aside className={`log-file-pane ${gridMode}`} aria-label="Files and hosts">
        <div className="host-selector"><div className="host-selector-heading"><span>HOSTS</span><button onClick={() => { setHosts(null); setOffset(0); setTailPage(false); }} aria-pressed={hosts === null}>all</button></div>
          <div className="host-selector-list">{view?.hosts.map((host,index) => <label key={host} title={host}><input type="checkbox" style={{accentColor:hostColors.get(host)}} aria-label={`Include host ${host}`} checked={selectedHosts.includes(host)} onChange={() => { setHosts((old) => { const current=old??view.hosts;return current.includes(host)?current.filter((item)=>item!==host):[...current,host]; });setOffset(0); setTailPage(false); }} /><span className="host-alias" style={{color:hostColors.get(host)}}>H{index+1}</span><span className="host-selector-name" style={{color:hostColors.get(host)}}>{host}</span></label>)}</div>
        </div>
        <LogGrid files={view?.files ?? []} today={today} selection={selection} marks={portMarks} opening={opening}
          transfers={gridTransfers}
          onActivate={activateGrid} onOpen={openLogs} onSelect={chooseGrid} onMarks={markPorts} />
      </aside>}
      <section className={`log-viewer number-mode-${numberMode} ${wrap ? 'wrap' : ''} ${merged ? 'merged' : ''} ${hasContentFilter ? 'has-content-filter' : ''}`} aria-label="Log records" aria-busy={loading}
        onPointerDown={focusLogs} onClick={focusLogs} onFocusCapture={focusLogs}>
        <div className="log-viewer-heading"><span>{mergeLabel}</span>
          <div className="log-filter-actions">
            <button className="log-tool" aria-label="Cycle line numbers" title="Cycle line numbers: off, file, overall (#)" onClick={cycleNumberMode}># {numberMode === 'source' ? 'File' : numberMode === 'overall' ? 'Overall' : 'Off'}</button>
            <button className="log-tool" aria-label="Filter in (fi)" aria-pressed={Boolean(include)} title={`Filter in (fi)${include ? `: ${include}` : ''}`} onClick={() => openFilter('include')}>Filter in <kbd>fi</kbd></button>
            <button className="log-tool" aria-label="Filter out (fo)" aria-pressed={Boolean(exclude)} title={`Filter out (fo)${exclude ? `: ${exclude}` : ''}`} onClick={() => openFilter('exclude')}>Filter out <kbd>fo</kbd></button>
            {(include || exclude || search) && <button className="log-tool" aria-label="Clear content filters (fc)" title="Clear content filters and search (fc)" onClick={clearFilters}><X size={12} /></button>}
          </div>
          <span>{loading ? <><LoaderCircle className="spin" size={10} />{indexProgress ? `Indexing ${indexProgress.current} · ${formatBytes(indexProgress.bytes)}` : 'Reading…'}</> : `${page?.total.toLocaleString()??0} records`}
          {!merged && shownHosts.length>1 && <button className="log-tool" aria-label="Maximize current host" aria-pressed={maximized} title="Maximize/restore current host (z)" onClick={() => setMaximized(!maximized)}>{maximized?'Restore':'Maximize'}</button>}</span></div>
        {filterEdit && <LogFilterDialog action={filterEdit.field} rules={filterRules} onChange={changeRules}
          onAction={(field) => setFilterEdit({ ...filterEdit, field })} onClose={closeFilter}
          position={filterPosition} onPosition={setFilterPosition} />}
        {!page?.rows.length ? <div className="logs-empty">
          {hasEarlierRecords && <div className="window-edge-row" aria-label="Earlier times"><span>Earlier</span>{([1,2,5,10,20,50,100,'all'] as const).map((amount)=><button key={amount} type="button" aria-label={`Extend before by ${amount === 'all' ? 'all' : `${amount} minutes`}`} onClick={()=>extendWindow('before',amount)}>{amount==='all'?'All':`${amount}m`}</button>)}</div>}
          {(!cursor || !visibleRows.length) && <div className="current-time-marker" aria-label="Current log time"><strong>{referenceTime ? formatWindowTime(referenceTime) : 'No timestamp selected'}</strong><span>{start||end?'Window center':'Current time'}</span></div>}
          <span>{opening?'Opening cached logs…':loading?'Reading log cache…':selectedFiles.some((file)=>file.cached)?'No records match this scope.':'Select a grid cell, then use Download or press d.'}</span>
          {hasLaterRecords && <div className="window-edge-row" aria-label="Later times"><span>Later</span>{([1,2,5,10,20,50,100,'all'] as const).map((amount)=><button key={amount} type="button" aria-label={`Extend after by ${amount === 'all' ? 'all' : `${amount} minutes`}`} onClick={()=>extendWindow('after',amount)}>{amount==='all'?'All':`${amount}m`}</button>)}<button type="button" className="window-live" aria-pressed={live} disabled={!live&&!selectedRemote.some((file)=>rotationOf(file.name)===0)} onClick={()=>{if(live)setLive(false);else{if(end)applyWindow(start,'');setFollow(true);setLive(true);}}}>Live</button></div>}
        </div>
          : <div className="host-panes">
            {visibleHosts.map((host,index) => {
              const rows=merged?visibleRows:visibleRows.filter((row)=>row.host===host);
              const bins=merged?page.timeline:page.hostTimeline?.[host]??page.timeline;
              const alias=merged?'':`H${(view?.hosts.indexOf(host)??0)+1}`;
              const bounds = page.availableHosts?.[host];
              const earlier = hasEarlierRecords && (merged || Boolean(bounds?.first && start && bounds.first < normalizeTime(start)!));
              const later = hasLaterRecords && (merged || Boolean(bounds?.last && end && bounds.last > normalizeTime(end)!));
              return <section className={`host-pane ${leadingHost===host?'leading-host':''}`} key={host} aria-label={`${host} log pane`} onClick={(event) => {setLeadingHost(host);focusLogs();if (window.getSelection()?.isCollapsed !== false && !(event.target as HTMLElement).closest('button')) panes.current[host]?.focus({preventScroll:true});}}>
                <div className="host-pane-title"><span className="host-alias" style={{color:hostColors.get(host)}}>{alias}</span><strong style={{color:hostColors.get(host)}}>{host}</strong><span>{merged?page.total:page.hosts.find((item)=>item.host===host)?.count??0} records</span></div>
                <ActivityStrip bins={bins} label={host} color={hostColors.get(host)} onSelect={selectTime} first={page.first} last={page.last} showAxis={index===0} />
                <div className="host-log-scroll" ref={(element)=>{panes.current[host]=element;}} tabIndex={0} aria-label={`Read ${host} logs`} onFocus={()=>{focusLogs();if(!merged)setLeadingHost(host);}}
                  onWheel={()=>{if(!merged)setLeadingHost(host);}} onScroll={(event)=>syncScroll(host,event.currentTarget)}>
                  {earlier && <div className="window-edge-row" aria-label="Earlier times"><span>Earlier</span>{([1,2,5,10,20,50,100,'all'] as const).map((amount)=><button key={amount} type="button" aria-label={`Extend before by ${amount === 'all' ? 'all' : `${amount} minutes`}`} onClick={()=>extendWindow('before',amount)}>{amount==='all'?'All':`${amount}m`}</button>)}</div>}
                  {(!cursor || !visibleRows.length) && <div className="current-time-marker" aria-label="Current log time"><strong>{referenceTime ? formatWindowTime(referenceTime) : 'No timestamp selected'}</strong><span>{start||end?'Window center':'Current time'}</span></div>}
                  {rows.length ? rows.map((row,rowIndex)=><LogRecord key={row.id} row={row} sourceNumber={row.ordinal ?? page.offset + rowIndex + 1} overallNumber={merged ? row.mergedOrdinal ?? page.offset + (mergedPositions.get(row.id) ?? rowIndex) + 1 : row.hostOrdinal ?? page.offset + rowIndex + 1} selected={cursor?.id===row.id} onSelect={selectRecord} showHost={merged} alias={`H${(view?.hosts.indexOf(row.host)??0)+1}`} hostColor={hostColors.get(row.host)} expanded={expanded===row.id} onContext={openLineMenu} onExpand={expandRecord} />)
                    : <div className="empty-host">No records in this page's time range.</div>}
                  {later && <div className="window-edge-row" aria-label="Later times"><span>Later</span>{([1,2,5,10,20,50,100,'all'] as const).map((amount)=><button key={amount} type="button" aria-label={`Extend after by ${amount === 'all' ? 'all' : `${amount} minutes`}`} onClick={()=>extendWindow('after',amount)}>{amount==='all'?'All':`${amount}m`}</button>)}<button type="button" className="window-live" aria-pressed={live} disabled={!live&&!selectedRemote.some((file)=>rotationOf(file.name)===0)} title="Refresh current logs and follow new records" onClick={()=>{if(live)setLive(false);else{if(end)applyWindow(start,'');setFollow(true);setLive(true);}}}>Live</button></div>}
                </div>
              </section>;
            })}
          </div>}
        <div className="log-page-controls"><span>{page?.rows.length?`${page.offset+1}–${Math.min(page.offset+pageAdvance,pageTotal)} / ${pageTotal.toLocaleString()}`:'0 records'}{!merged?' per host':''}</span><div><button aria-label="Previous log page" disabled={!page||page.offset===0||loading} onClick={()=>{setFollow(false);setTailPage(false);setOffset(Math.max(0,(page?.offset??0)-pageSize));}}><ChevronLeft size={13}/></button><button aria-label="Next log page" disabled={!page||page.offset+pageAdvance>=pageTotal||loading} onClick={()=>{setFollow(false);setTailPage(false);setOffset((page?.offset??0)+pageAdvance);}}><ChevronRight size={13}/></button><button className="log-tool" disabled={!page?.total||loading} onClick={()=>setFollow(true)}>{follow?<Pause size={10}/>:<Play size={10}/>}Latest</button></div></div>
      </section>
    </div>
    <footer className="log-status" role="status"><span>{job?<><LoaderCircle className="spin" size={10}/>{job} · {progress?.current||'Preparing…'} · {progress?.completed??0}/{progress?.total||'…'} · {formatBytes(progress?.bytes??0)}{downloads.length > 1 ? ` · ${downloads.length-1} queued` : ''}<button onClick={cancelDownloads}>Cancel</button></>:<>{live?'↻ live':'↓ on demand'} · {scopeLabel(selection)} · {selectedFiles.length} sources{referenceHost?<> · lead <span style={{color:hostColors.get(referenceHost)}}>{referenceHost}</span></>:null}{include ? ` · in: ${include}` : ''}{exclude ? ` · out: ${exclude}` : ''}</>}</span><span>{start||end?<button onClick={()=>applyWindow('','')}>Window: {start||'…'} → {end||'…'} ×</button>:null}{view?.warnings.length||page?.warnings.length?<details><summary>Cache warnings</summary>{[...(view?.warnings??[]),...(page?.warnings??[])].map((message,index)=><pre key={index}>{message}</pre>)}</details>:null}<span>{filterPrefixPending ? 'f: follow · fi: filter in · fo: filter out · fc: clear' : 'j/k: move · u/d: page · Nd/Nu: minutes · Nhd/Nhu: hours · fi/fo: filters'}</span></span></footer>
  </div>;
}

function RefreshCountdown({ next, interval, refreshing }: { next:number; interval:number; refreshing:boolean }) {
  const [now,setNow]=useState(Date.now);
  useEffect(()=>{setNow(Date.now());const timer=setInterval(()=>setNow(Date.now()),250);return()=>clearInterval(timer);},[next]);
  const remaining=Math.max(0,Math.ceil((next-now)/1000));
  const progress=Math.max(0,Math.min(1,(next-now)/interval));
  return <span className="log-refresh-countdown" aria-label={refreshing ? 'Refreshing logs' : `Next log refresh in ${remaining} seconds`} title={refreshing ? 'Refreshing logs' : `Next refresh in ${remaining}s`}>
    {refreshing ? <LoaderCircle className="spin" size={12}/> : <span className="log-refresh-ring" style={{background:`conic-gradient(#9dd7c3 ${progress*360}deg, #ffffff20 0)`}} aria-hidden="true"/>}{refreshing ? 'Refreshing' : `${remaining}s`}
  </span>;
}

const LogRecord = memo(function LogRecord({ row, sourceNumber, overallNumber, selected, onSelect, onContext, expanded, onExpand, showHost, alias, hostColor }: { row: LogRow; sourceNumber: number; overallNumber: number; selected: boolean; onSelect: (row: LogRow) => void; onContext: (event: React.MouseEvent, row: LogRow) => void; expanded: boolean; onExpand: (id: string) => void; showHost: boolean; alias: string; hostColor?: string }) {
  const detail = useMemo(() => expanded ? formatLogDetail(row.raw) : '', [expanded, row.raw]);
  const [copyStatus, setCopyStatus] = useState('');
  async function copyDetail() {
    try { await navigator.clipboard.writeText(detail); setCopyStatus('Copied'); }
    catch { setCopyStatus('Copy failed'); }
  }
  return <article className={`log-record ${row.level.toLowerCase()} ${row.matched?'matched':'context'} ${selected?'selected-record':''}`} data-time={row.time??''} aria-current={selected ? true : undefined} data-record-id={row.id} onClick={()=>onSelect(row)} onContextMenu={(event)=>onContext(event,row)}>
    <button className="inspect-record" onClick={()=>onExpand(row.id)} aria-expanded={expanded} title={`${row.host} / ${row.file} · original record`} aria-label={`Inspect ${row.host} ${row.time??''} ${row.level} record`}>{expanded?'⌄':'›'}</button>
    <span className="record-line-number source-number" aria-label={`File line ${sourceNumber}`} title={`Source record ${sourceNumber} in ${row.file}`}>{sourceNumber}</span>
    <span className="record-line-number overall-number" aria-label={`${showHost ? 'Merged line' : 'Host line'} ${overallNumber}`} title={`${showHost ? 'Merged' : 'Host'} record ${overallNumber} · source record ${sourceNumber} in ${row.file}`}>{overallNumber}</span>
    {showHost&&<span className="record-host" style={{color:hostColor}} title={row.host}>{alias}</span>}<pre className={`record-body ${row.lines ? 'styled-record' : ''}`}>{row.lines ? row.lines.map((line,index)=><span key={index}>{index > 0 && '\n'}{line.map((part,partIndex)=><span key={partIndex} className={part.color && /^#[0-9a-f]{6}$/i.test(part.color) ? `log-color-${part.color.slice(1).toLowerCase()}` : undefined} style={part.color ? {color: part.color} : undefined}>{part.text}</span>)}</span>) : row.text.startsWith(`${row.kind} `) ? <><span className={`log-kind-${row.kind.toLowerCase()}`}>{row.kind}</span>{row.text.slice(row.kind.length)}</> : row.text}</pre>
    {expanded&&<div className="record-detail" onClick={(event) => event.stopPropagation()}><div className="record-detail-heading"><span><span style={{color:hostColor}}>{row.host}</span> / {row.file} · record detail</span><span role="status">{copyStatus}</span><button className="log-tool" onClick={() => void copyDetail()} aria-label={`Copy record from ${row.host}`}>Copy</button></div><pre>{detail}</pre></div>}
  </article>;
});
const ActivityStrip = memo(function ActivityStrip({ bins,label,color,onSelect,first,last,showAxis }: { bins: {time:string;count:number}[]; label:string; color?:string; onSelect:(first:number,last:number)=>void; first:string|null;last:string|null;showAxis:boolean }) {
  const initial=useRef<number|null>(null);
  const maximum=Math.max(1,...bins.map((bin)=>bin.count));
  return <div className="host-activity" aria-label={`${label} activity`} style={color ? {'--host-color': color} as CSSProperties : undefined}>
    <div className="activity-strip">{bins.map((bin,index)=><button key={bin.time} title={`${label} · ${bin.time.replace('T',' ')} · ${bin.count} records`} aria-label={`${label} activity ${bin.time}: ${bin.count} records`}
      style={{height:`${bin.count?Math.max(12,bin.count/maximum*100):3}%`}} onPointerDown={()=>{initial.current=index;}} onPointerUp={()=>{if(initial.current!==null){onSelect(initial.current,index);initial.current=null;}}} onClick={(event)=>{if(event.detail===0)onSelect(index,index);}}/> )}</div>
    {showAxis&&<div className="activity-axis"><time>{first?.replace('T',' ')??'—'}</time><time>{last?.replace('T',' ')??'—'}</time></div>}
  </div>;
});
function normalizeTime(value: string) { if (!value) return null; return value.length === 16 ? `${value}:00.000` : value.length === 19 ? `${value}.000` : value; }
function shiftTime(value: string, milliseconds: number) {
  const result = new Date(`${normalizeTime(value)}Z`).getTime() + milliseconds;
  if (!Number.isFinite(result) || Math.abs(result) > 8.64e15) return null;
  return new Date(result).toISOString().slice(0, 23);
}
function formatWindowTime(value: string) { return (normalizeTime(value) ?? value).replace('T', ' '); }
function midpointTime(start: string, end: string) {
  const first = Date.parse(`${normalizeTime(start)}Z`), last = Date.parse(`${normalizeTime(end)}Z`);
  return Number.isFinite(first) && Number.isFinite(last) ? new Date(first + (last - first) / 2).toISOString().slice(0, 23) : null;
}
export function formatBytes(bytes: number) { const units = ['B', 'KiB', 'MiB', 'GiB']; let index = 0; while (bytes >= 1024 && index < units.length - 1) { bytes /= 1024; index++; } return `${index ? bytes.toFixed(1) : bytes} ${units[index]}`; }
