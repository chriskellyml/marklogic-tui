import type { LogFile } from './logs-api';

export const LOG_KINDS = ['ERR', 'ACC', 'REQ'] as const;
export type LogKind = typeof LOG_KINDS[number];
export interface GridCursor { port: string; day: string; kind: LogKind }
export interface GridSelection extends GridCursor { mode: 'cell' | 'row' | 'column' | 'day' | 'port' | 'cells'; cells?: GridCursor[]; ports?: string[] }
export type PortMarks = Record<string, 'include' | 'exclude'>;
export function addDays(day: string, count: number) {
  const date = new Date(`${day}T12:00:00Z`); date.setUTCDate(date.getUTCDate() + count); return date.toISOString().slice(0, 10);
}
export function serverDay(now: string) {
  if (/^\d{4}-\d{2}-\d{2}/.test(now)) return now.slice(0, 10);
  const date = new Date(); return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
}
export function currentServerDay(sample: string, clock: number) {
  const offset = sample.match(/([+-])(\d\d):(\d\d)$/);
  if (offset) {
    const minutes = (Number(offset[2]) * 60 + Number(offset[3])) * (offset[1] === '+' ? 1 : -1);
    return new Date(clock + minutes * 60_000).toISOString().slice(0, 10);
  }
  if (sample.endsWith('Z')) return new Date(clock).toISOString().slice(0, 10);
  return serverDay('');
}
export function rotationOf(name: string) { return Number(name.match(/_(\d+)\.txt$/)?.[1] ?? 0); }
export function portLabel(port: string) { return port === '----' ? 'ROOT' : port; }
export function fileDays(file: LogFile, today: string) {
  const days = new Set(file.days ?? []);
  if (file.remote) days.add(addDays(today, -rotationOf(file.name)));
  if (!days.size) days.add(/^\d{4}-\d{2}-\d{2}/.test(file.modified) ? file.modified.slice(0, 10) : addDays(today, -rotationOf(file.name)));
  return [...days].sort();
}
export function comparePorts(a: string, b: string) {
  const rank = (port: string) => port === '----' ? -1 : /^\d+$/.test(port) ? Number(port) : Number.MAX_SAFE_INTEGER;
  return rank(a) - rank(b) || a.localeCompare(b);
}
export function portAllowed(port: string, marks: PortMarks) {
  return marks[port] !== 'exclude' && (!Object.values(marks).includes('include') || marks[port] === 'include');
}
export function selectionContains(selection: GridSelection | null, cell: GridCursor) {
  if (!selection) return false;
  switch (selection.mode) {
    case 'row': return selection.port === cell.port && selection.kind === cell.kind;
    case 'port': return selection.port === cell.port;
    case 'column': return selection.day === cell.day && selection.kind === cell.kind;
    case 'day': return selection.day === cell.day;
    case 'cells': return selection.cells?.some((item) => cellKey(item) === cellKey(cell)) ?? false;
    default: return cellKey(selection) === cellKey(cell);
  }
}
export function cellKey(cell: GridCursor) { return `${cell.port}|${cell.day}|${cell.kind}`; }
export function selectedSources(files: LogFile[], selection: GridSelection | null, today: string, marks: PortMarks = {}) {
  const result: LogFile[] = [];
  const scopes: { source: string; day: string }[] = [];
  if (!selection) return { files: result, scopes };
  for (const file of files) {
    if (!LOG_KINDS.includes(file.kind as LogKind) || !portAllowed(file.port, marks)) continue;
    if ((selection.mode === 'column' || selection.mode === 'day') && selection.ports && !selection.ports.includes(file.port)) continue;
    if ((selection.mode === 'row' || selection.mode === 'port') && selection.port === file.port &&
      (selection.mode === 'port' || selection.kind === file.kind)) { result.push(file); continue; }
    const days = fileDays(file, today).filter((day) => selectionContains(selection, { port: file.port, day, kind: file.kind as LogKind }));
    if (!days.length) continue;
    result.push(file); for (const day of days) scopes.push({ source: file.id, day });
  }
  return { files: result, scopes };
}
export function defaultGridSelection(files: LogFile[], today: string): GridSelection | null {
  const usable = files.filter((file) => LOG_KINDS.includes(file.kind as LogKind));
  const file = usable.find((file) => file.cached && file.name === 'ErrorLog.txt') ?? usable.find((file) => file.cached) ?? usable.find((file) => file.name === 'ErrorLog.txt') ?? usable[0];
  if (!file) return null;
  const cachedDays = usable.filter((item) => item.cached && item.port === file.port && item.kind === file.kind)
    .flatMap((item) => item.days ?? []).sort();
  const days = cachedDays.length ? cachedDays : fileDays(file, today);
  return { mode: 'cell', port: file.port, kind: file.kind as LogKind, day: days.includes(today) ? today : days[days.length - 1] };
}
export function scopeLabel(selection: GridSelection | null) {
  if (!selection) return 'Select a grid cell';
  const kind = { ERR: 'Error', ACC: 'Access', REQ: 'Request' }[selection.kind];
  if (selection.mode === 'row') return `${portLabel(selection.port)} · ${kind} · all days`;
  if (selection.mode === 'port') return `${portLabel(selection.port)} · all log types and days`;
  if (selection.mode === 'column') return `${selection.day} · ${kind} · all ports`;
  if (selection.mode === 'day') return `${selection.day} · all ports and log types`;
  if (selection.mode === 'cells') return `${selection.cells?.length ?? 0} selected cells`;
  return `${portLabel(selection.port)} · ${kind} · ${selection.day}`;
}
