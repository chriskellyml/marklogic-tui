import { memo, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { ChevronLeft, ChevronRight, Search } from 'lucide-react';
import type { LogFile, LogProgress } from './logs-api';
import { LOG_KINDS, addDays, cellKey, comparePorts, fileDays, portAllowed, portLabel, scopeLabel, selectionContains,
  type GridCursor, type GridSelection, type LogKind, type PortMarks } from './log-grid';

const labels = { ERR: 'Error', ACC: 'Access', REQ: 'Request' };
export interface GridTransfer { selection: GridSelection; marks: PortMarks; files: LogFile[]; active: boolean; progress: LogProgress | null }
interface Props { files: LogFile[]; today: string; selection: GridSelection | null; marks: PortMarks;
  onSelect: (selection: GridSelection) => void; onMarks: (marks: PortMarks) => void; opening: boolean;
  transfers?: GridTransfer[]; compact?: boolean; onActivate?: () => void; onOpen?: () => void }
export const LogGrid = memo(function LogGrid({ files, today, selection, marks, onSelect, onMarks, opening, transfers = [], compact = false, onActivate, onOpen }: Props) {
  const dayCount = 7;
  const [firstDay, setFirstDay] = useState(today);
  const [filter, setFilter] = useState('');
  const [cursor, setCursor] = useState<GridCursor | null>(selection);
  const cellRefs = useRef<Record<string, HTMLButtonElement | null>>({});
  const model = useMemo(() => {
    const cells = new Map<string, LogFile[]>();
    const ports = new Set<string>();
    for (const file of files) {
      if (!LOG_KINDS.includes(file.kind as LogKind)) continue;
      const needle = filter.toLowerCase();
      if (needle && !`${file.port} ${portLabel(file.port)} ${file.name} ${file.host}`.toLowerCase().includes(needle)) continue;
      ports.add(file.port);
      for (const day of fileDays(file, today)) {
        const key = cellKey({ port: file.port, day, kind: file.kind as LogKind });
        const existing = cells.get(key); if (existing) existing.push(file); else cells.set(key, [file]);
      }
    }
    return { cells, ports: [...ports].sort(comparePorts) };
  }, [files, today, filter]);
  const days = compact ? [selection?.day ?? today] : Array.from({ length: dayCount }, (_, index) => addDays(firstDay, -index));
  const previousToday = useRef(today);
  useEffect(() => {
    const previous = previousToday.current;
    setFirstDay((day) => day === previous ? today : day);
    previousToday.current = today;
  }, [today]);
  useEffect(() => {
    if (!selection) { setCursor(null); return; }
    setCursor({ port: selection.port, kind: selection.kind, day: selection.day });
  }, [selection]);
  function reveal(day: string) {
    if (day > firstDay) setFirstDay(day);
    else if (day < addDays(firstDay, -(dayCount - 1))) setFirstDay(addDays(day, dayCount - 1));
  }
  useEffect(() => { if (selection?.mode === 'cell') reveal(selection.day); }, [selection?.day, selection?.mode]);
  function choose(cell: GridCursor, mode: GridSelection['mode'] = 'cell') {
    setCursor(cell); reveal(cell.day); onSelect({ ...cell, mode, ...((mode === 'column' || mode === 'day') ? { ports: model.ports } : {}) });
  }
  function toggle(cell: GridCursor) {
    const selected = selection?.mode === 'cells' ? selection.cells ?? [] : selection?.mode === 'cell' ? [selection] : [];
    const cells = selected.some((item) => cellKey(item) === cellKey(cell)) ? selected.filter((item) => cellKey(item) !== cellKey(cell)) : [...selected, cell];
    setCursor(cell); onSelect({ ...cell, mode: 'cells', cells });
  }
  function keyboard(event: KeyboardEvent<HTMLDivElement>) {
    if (event.target instanceof HTMLInputElement || event.metaKey || event.ctrlKey || event.altKey || !model.ports.length) return;
    const current = cursor ?? { port: model.ports[0], kind: 'ERR' as LogKind, day: firstDay };
    const next = { ...current };
    let index = LOG_KINDS.indexOf(current.kind);
    switch (event.key) {
      case 'ArrowRight': case 'l': index++; if (index === 3) { next.day = addDays(next.day, -1); index = 0; } next.kind = LOG_KINDS[index]; break;
      case 'ArrowLeft': case 'h': index--; if (index < 0) { next.day = addDays(next.day, 1); index = 2; } next.kind = LOG_KINDS[index]; break;
      case 'ArrowDown': case 'j': next.port = model.ports[Math.min(model.ports.length - 1, Math.max(0, model.ports.indexOf(current.port)) + 1)]; break;
      case 'ArrowUp': case 'k': next.port = model.ports[Math.max(0, model.ports.indexOf(current.port) - 1)]; break;
      case 'PageDown': next.day = addDays(next.day, -1); break;
      case 'PageUp': next.day = addDays(next.day, 1); break;
      case 'Enter': event.preventDefault(); if (!selection) choose(current); onOpen?.(); return;
      case 'V': case 'v': event.preventDefault(); choose(current, 'row'); return;
      case 'C': case 'c': event.preventDefault(); choose(current, 'column'); return;
      case ' ': if (!(event.target instanceof HTMLButtonElement) || !event.target.classList.contains('grid-file-status')) return; event.preventDefault(); toggle(current); return;
      case 'Escape': event.preventDefault(); choose(current); return;
      default: return;
    }
    event.preventDefault(); event.stopPropagation();
    if (next.day > today) return;
    choose(next);
    requestAnimationFrame(() => cellRefs.current[cellKey(next)]?.focus());
  }
  function mark(port: string) {
    const next = { ...marks };
    if (!next[port]) next[port] = 'include'; else if (next[port] === 'include') next[port] = 'exclude'; else delete next[port];
    onMarks(next);
  }
  const active = cursor ?? selection ?? { port: model.ports[0] ?? '----', kind: 'ERR' as LogKind, day: firstDay };
  const hasMarks = Object.keys(marks).length > 0;
  return <section className={`log-grid-panel ${compact ? 'compact' : ''}`} aria-label="Port and day log grid"
    onClick={() => onActivate?.()} onFocusCapture={(event) => { if (event.target.matches(':focus-visible')) onActivate?.(); }}>
    {compact ? <div className="grid-compact-heading"><button aria-label="Expand files" onClick={() => onActivate?.()}>Files</button><span>{selection?.day.slice(5) ?? today.slice(5)}</span></div> : <div className="grid-toolbar">
      <label className="grid-search"><Search size={11} /><input aria-label="Filter log grid" placeholder="Port / file / host" value={filter} onChange={(e) => setFilter(e.target.value)} /></label>
      <div className="grid-date-navigation"><button aria-label="Newer log days" disabled={firstDay >= today} onClick={() => setFirstDay(addDays(firstDay, dayCount) > today ? today : addDays(firstDay, dayCount))}><ChevronLeft size={13} /></button>
        <button title="Current day" onClick={() => setFirstDay(today)}>Today</button><input aria-label="First grid day" type="date" max={today} value={firstDay} onChange={(e) => { if (e.target.value) setFirstDay(e.target.value); }} />
        <button aria-label="Older log days" onClick={() => setFirstDay(addDays(firstDay, -dayCount))}><ChevronRight size={13} /></button></div>
    </div>}
    <div className="log-grid-scroll" onKeyDown={keyboard}>
      <table className="log-file-grid" style={{ width: 76 + days.length * 60 }} aria-label="MarkLogic logs by port, day and type">
        <thead><tr><th className="grid-port-heading" rowSpan={2} scope="col">PORT</th>
          {days.map((day) => <th className={`grid-day-heading ${selection?.day === day ? 'current-day' : ''}`} scope="colgroup" colSpan={3} key={day}>
            <button aria-label={`Select all logs on ${day}`} onClick={() => choose({ ...active, day }, 'day')}><time>{day.slice(5)}</time></button></th>)}
        </tr><tr>{days.flatMap((day) => LOG_KINDS.map((kind) => <th className="grid-kind-heading" scope="col" key={`${day}-${kind}`}>
          <button className={selection?.mode === 'column' && selection.day === day && selection.kind === kind ? 'selected-column' : ''}
            aria-label={`Select ${labels[kind]} column ${day}`} title={`${labels[kind]} logs on ${day} across all ports (C)`}
            onClick={() => choose({ ...active, day, kind }, 'column')}>{kind === 'ERR' ? 'E' : kind === 'ACC' ? 'A' : 'R'}</button></th>))}</tr></thead>
        <tbody>{model.ports.map((port) => <tr key={port} className={`${active.port === port ? 'cursor-row' : ''} ${!portAllowed(port, marks) ? 'filtered-port' : ''}`}>
          <th scope="row" className="grid-port-label"><button className={`grid-port-mark ${marks[port] ?? ''}`} aria-label={`Filter port ${portLabel(port)}`} title="Cycle port filter: include (+), exclude (−), clear" onClick={() => mark(port)}>{marks[port] === 'include' ? '+' : marks[port] === 'exclude' ? '−' : '·'}</button>
            <button className="grid-select-row" aria-label={`Select port ${portLabel(port)} ${labels[active.kind]} across all days`} aria-pressed={selection?.mode === 'row' && selection.port === port} title={`Select ${portLabel(port)} ${labels[active.kind]} logs across all days (V)`}
              onClick={() => choose({ port, day: active.day, kind: active.kind }, 'row')}>{portLabel(port)}</button></th>
          {days.flatMap((day) => LOG_KINDS.map((kind) => {
            const cell = { port, day, kind }; const key = cellKey(cell); const sources = model.cells.get(key) ?? [];
            const hosts = new Set(sources.map((file) => file.host)).size;
            const full = sources.length > 0 && sources.every((file) => file.complete && !file.stale);
            const cached = sources.some((file) => file.cached); const empty = sources.length > 0 && sources.every((file) => file.size === 0);
            const status = empty ? 'empty' : full ? 'complete' : cached ? 'partial' : sources.length ? 'remote' : 'missing';
            let transfer = '';
            for (const task of transfers) {
              if (!portAllowed(port, task.marks) || !selectionContains(task.selection, cell) ||
                ((task.selection.mode === 'column' || task.selection.mode === 'day') && task.selection.ports && !task.selection.ports.includes(port))) continue;
              if (!task.active || !task.progress || !['download', 'tail'].includes(task.progress.phase)) {
                if (task.active) { transfer = 'active'; break; }
                transfer ||= 'queued'; continue;
              }
              const ordered = [...task.files].sort((a, b) => a.id.localeCompare(b.id));
              const remaining = ordered.slice(task.progress.completed).filter((file) => file.port === port && file.kind === kind && fileDays(file, today).includes(day));
              if (remaining.some((file) => task.progress?.current === `${file.host} / ${file.name}`)) { transfer = 'active'; break; }
              if (remaining.length) transfer ||= 'queued';
            }
            const selected = selectionContains(selection, cell) && portAllowed(port, marks);
            const focused = cellKey(active) === key;
            const description = `${portLabel(port)} ${labels[kind]} ${day}`;
            const detail = `${hosts} hosts · ${sources.filter((file) => file.complete && !file.stale).length} complete · ${sources.filter((file) => file.cached).length} cached`;
            return <td key={key} className={`grid-cell ${day === active.day ? 'cursor-day' : ''} ${selected ? 'scope-selected' : ''} ${focused ? 'cursor-cell' : ''}`}>
              <button ref={(element) => { cellRefs.current[key] = element; }} aria-label={`${description}: ${sources.length ? detail : 'no file'}${transfer ? ` · download ${transfer}` : ''}`} aria-pressed={selected}
                tabIndex={focused ? 0 : -1} className={`grid-file-status ${status} ${transfer ? `download-${transfer}` : ''}`} title={`${description}\n${sources.length ? sources.map((file) => `${file.host}: ${file.name} (${file.cachedBytes.toLocaleString()} cached bytes)`).join('\n') : 'No known file for this cell'}${transfer ? `\nDownload ${transfer}` : ''}\nClick: cell · Shift-click / Space: add cell`}
                onDoubleClick={() => { choose(cell); onOpen?.(); }} onFocus={() => setCursor(cell)} onClick={(event) => event.shiftKey ? toggle(cell) : choose(cell)}>{transfer === 'active' ? '↓' : transfer === 'queued' ? 'Q' : status === 'missing' ? '—' : status === 'empty' ? '·' : status === 'complete' ? '●' : status === 'partial' ? '◐' : '↓'}</button>
            </td>;
          }))}
        </tr>)}</tbody>
      </table>
      {!model.ports.length && <div className="empty-grid">{opening ? 'Loading the remembered grid…' : files.length ? 'No ports match the grid filter.' : 'Discover logs to populate the port × day grid. Cached files appear immediately.'}</div>}
    </div>
    <div className="grid-footer"><span className="grid-scope-label" title={scopeLabel(selection)}>{scopeLabel(selection)}</span>
      {!compact && <><span className="grid-legend"><i className="complete">●</i> fresh <i className="partial">◐</i> stale <i className="remote">↓</i> missing <i className="empty">·</i> empty <i className="queued">Q</i> queued <i className="active">↓</i> downloading</span>
      <span className="grid-keys">Enter: logs · d: download · V: row · C: column · \: files</span>{hasMarks && <button className="grid-reset" onClick={() => onMarks({})}>Clear ports</button>}</>}
    </div>
  </section>;
});
