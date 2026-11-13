import { describe, expect, it } from 'vitest';
import { currentServerDay, defaultGridSelection, fileDays, selectedSources, type GridSelection } from './log-grid';
import type { LogFile } from './logs-api';
const today = '2026-11-08';
function file(id: string, port: string, kind: string, day: string, host = 'host-a'): LogFile {
  return { id, port, kind, host, name: `${port}_${kind}Log.txt`, days: [day], modified: '', updatedAt: null,
    size: 10, cachedBytes: 10, complete: true, cached: true, stale: false, remote: false };
}
const files = [file('a', '8080', 'ERR', today), file('b', '8080', 'ERR', '2026-11-07'),
  file('c', '8080', 'ACC', today), file('d', '8081', 'ERR', today), file('e', '8080', 'ERR', today, 'host-b')];
const cell: GridSelection = { mode: 'cell', port: '8080', kind: 'ERR', day: today };
describe('MarkLogic grid scope', () => {
  it('uses the current date in the server offset even when inventory is from yesterday', () => {
    expect(currentServerDay('2026-11-08T11:00:00+01:00', Date.parse('2026-11-09T23:30:00Z'))).toBe('2026-11-10');
    expect(currentServerDay('2026-11-08T11:00:00-05:00', Date.parse('2026-11-10T02:00:00Z'))).toBe('2026-11-09');
  });
  it('selects a port/type cell across hosts and constrains records to its calendar day', () => {
    expect(selectedSources(files, cell, today)).toEqual({ files: [files[0], files[4]], scopes: [{ source: 'a', day: today }, { source: 'e', day: today }] });
  });
  it('V selects the current port and kind across all days', () => {
    expect(selectedSources(files, { ...cell, mode: 'row' }, today)).toEqual({ files: [files[0], files[1], files[4]], scopes: [] });
  });
  it('C selects the current day and kind across ports, excluding other kinds and days', () => {
    const selected = selectedSources(files, { ...cell, mode: 'column' }, today);
    expect(selected.files.map((file) => file.id)).toEqual(['a', 'd', 'e']);
    expect(selected.scopes.every((scope) => scope.day === today)).toBe(true);
    expect(selectedSources(files, { ...cell, mode: 'column', ports: ['8080'] }, today).files.map((file) => file.id)).toEqual(['a', 'e']);
  });
  it('includes a physical log in every known calendar day it spans', () => {
    const spanning = { ...files[0], days: ['2026-11-07', today] };
    expect(selectedSources([spanning], { ...cell, day: '2026-11-07' }, today).scopes).toEqual([{ source: 'a', day: '2026-11-07' }]);
  });
  it('applies include/exclude port marks to a column selection', () => {
    expect(selectedSources(files, { ...cell, mode: 'column' }, today, { '8080': 'exclude' }).files.map((file) => file.id)).toEqual(['d']);
    expect(selectedSources(files, { ...cell, mode: 'column' }, today, { '8080': 'include' }).files.map((file) => file.id)).toEqual(['a', 'e']);
  });
  it('uses remote rotation numbers as date hints and preserves older cached calendar days', () => {
    expect(fileDays({ ...files[0], name: '8080_ErrorLog_2.txt', remote: true, days: ['2026-10-31'] }, today)).toEqual(['2026-10-31', '2026-11-06']);
    expect(defaultGridSelection([{ ...files[0], days: ['2026-11-05'] }], today)?.day).toBe('2026-11-05');
  });
  it('opens a cached day instead of a remote-only today when the app starts', () => {
    const cached = { ...files[0], remote: true, days: ['2026-11-05'] };
    const selection = defaultGridSelection([cached], today);
    expect(selection?.day).toBe('2026-11-05');
    expect(selectedSources([cached], selection, today).scopes).toEqual([{ source: cached.id, day: '2026-11-05' }]);
  });
});
