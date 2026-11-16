import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { LogsWorkspace } from './LogsWorkspace';
import { logsApi, type LogPage, type LogView } from './logs-api';

vi.mock('./logs-api', () => ({ logsApi: {
  archive: vi.fn(), saveArchiveSettings: vi.fn(), validateFilterRules: vi.fn(), health: vi.fn(), open: vi.fn(), discover: vi.fn(), download: vi.fn(), page: vi.fn(), cancel: vi.fn(), close: vi.fn(), cancelSearch: vi.fn(),
} }));
const view: LogView = {
  session: 1, hosts: ['host-a', 'host-b'], now: '2026-11-08T12:00:00+01:00', cacheRoot: '/cache', warnings: [], errors: {},
  files: ['a', 'b'].map((suffix) => ({ id: suffix, host: `host-${suffix}`, name: 'ErrorLog.txt', kind: 'ERR', port: '----',
    size: 500, cachedBytes: 500, cached: true, complete: true, stale: false, remote: true, modified: '', updatedAt: null })),
};
const page: LogPage = {
  rows: ['a', 'b'].map((suffix) => ({ id: `${suffix}:1`, source: suffix, host: `host-${suffix}`, file: 'ErrorLog.txt',
    time: '2026-11-08T10:00:00.000', level: 'Error', kind: 'ERR', port: '----', text: `message from host-${suffix}`,
    raw: `raw from host-${suffix}`, matched: true })),
  total: 2, offset: 0, hosts: [{ host: 'host-a', count: 1 }, { host: 'host-b', count: 1 }],
  timeline: [{ time: '2026-11-08T10:00:00.000', count: 2 }], first: '2026-11-08T10:00:00.000', last: '2026-11-08T10:00:00.000', warnings: [],
};
const props = { active: true, disabled: false, servers: [{ name: 'local', endpoint: 'http://localhost:8000', appServers: [] }],
  connection: { server: 'local', appServer: null, database: 'Documents' }, onServer: vi.fn(), onAppServer: vi.fn() };

beforeEach(() => {
  vi.clearAllMocks();
  window.localStorage.clear();
  vi.mocked(logsApi.validateFilterRules).mockResolvedValue(undefined);
  vi.setSystemTime(new Date("2026-11-08T11:00:00Z"));
  vi.mocked(logsApi.health).mockResolvedValue("2026-11-08T12:00:00+01:00");
  vi.mocked(logsApi.open).mockResolvedValue(view);
  vi.mocked(logsApi.page).mockResolvedValue(page);
  vi.mocked(logsApi.discover).mockResolvedValue(view);
  vi.mocked(logsApi.download).mockResolvedValue(view);
  vi.mocked(logsApi.cancel).mockResolvedValue(undefined);
  vi.mocked(logsApi.close).mockResolvedValue(undefined);
  vi.mocked(logsApi.cancelSearch).mockResolvedValue(undefined);
});
async function ready() {
  render(<LogsWorkspace {...props} />);
  await screen.findByText('message from host-a');
}

describe('desktop log workspace', () => {
  it('archives without immediately rebuilding the index and resumes on explicit read', async () => {
    vi.mocked(logsApi.archive).mockResolvedValue({ view, report: { compressed: 2, archived: 1, retiredBytes: 1000, indexBytesReclaimed: 2000, warnings: [] } });
    await ready();
    await userEvent.click(screen.getByRole('button', { name: /^Archive$/ }));
    fireEvent.change(screen.getByLabelText('Archive folder'), { target: { value: '/Volumes/Archive' } });
    const reads = vi.mocked(logsApi.page).mock.calls.length;
    await userEvent.click(screen.getByRole('button', { name: 'Archive now' }));
    await screen.findByRole('button', { name: 'Read selected logs' });
    expect(logsApi.archive).toHaveBeenCalledWith(1, { folder: '/Volumes/Archive', compressAfterDays: 7, archiveAfterDays: 7 }, expect.any(Function));
    expect(logsApi.page).toHaveBeenCalledTimes(reads);
    await userEvent.click(screen.getByRole('button', { name: 'Read selected logs' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenCalledTimes(reads + 1));
  });
  it('moves by minutes with counted d and u commands', async () => {
    const rows = [0, 1, 2, 5, 7].map((minute) => ({ ...page.rows[0], id: `a:${minute}`,
      time: `2026-11-08T10:0${minute}:00.000`, text: `minute ${minute}` }));
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, rows, total: rows.length, pageTotal: rows.length,
      hosts: [{ host: 'host-a', count: rows.length }], first: rows[0].time, last: rows[rows.length - 1].time });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('minute 0');
    const pane = screen.getByLabelText('Read host-a logs');
    fireEvent.focus(pane);
    fireEvent.keyDown(pane, { key: '5' });
    fireEvent.keyDown(pane, { key: 'd' });
    expect(screen.getByText('minute 5').closest('article')).toHaveAttribute('aria-current', 'true');
    fireEvent.keyDown(pane, { key: '5' });
    fireEvent.keyDown(pane, { key: 'u' });
    expect(screen.getByText('minute 0').closest('article')).toHaveAttribute('aria-current', 'true');
  });
  it('selects the requested time after loading a different page', async () => {
    const row = (minute: number) => ({ ...page.rows[0], id: `jump:${minute}`,
      time: `2026-11-08T10:0${minute}:00.000`, text: `jump minute ${minute}` });
    const firstPage = { ...page, rows: [row(0), row(1)], total: 8, pageTotal: 8,
      hosts: [{ host: 'host-a', count: 8 }], first: row(0).time, last: row(7).time,
      availableFirst: row(0).time, availableLast: row(7).time };
    vi.mocked(logsApi.page).mockImplementation(async (_session, query) => query.start
      ? { ...firstPage, rows: [row(4), row(5), row(6)], total: 3, pageTotal: 3, hosts: [{ host: 'host-a', count: 3 }] }
      : firstPage);
    render(<LogsWorkspace {...props} />);
    await screen.findByText('jump minute 0');
    const pane = screen.getByLabelText('Read host-a logs');
    fireEvent.focus(pane);
    fireEvent.keyDown(pane, { key: '5' });
    fireEvent.keyDown(pane, { key: 'd' });
    await waitFor(() => expect(screen.getByText('jump minute 5').closest('article')).toHaveAttribute('aria-current', 'true'));
  });
  it('does not offer earlier records on a host already at its earliest time', async () => {
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, availableFirst: '2026-11-08T09:00:00.000',
      availableLast: '2026-11-08T10:00:00.000', availableHosts: {
        'host-a': { first: '2026-11-08T10:00:00.000', last: '2026-11-08T10:00:00.000' },
        'host-b': { first: '2026-11-08T09:00:00.000', last: '2026-11-08T10:00:00.000' },
      } });
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Options' }));
    fireEvent.change(screen.getByLabelText('Log start time'), { target: { value: '2026-11-08T09:59:00' } });
    await userEvent.click(screen.getByRole('button', { name: 'Close log options' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ start: '2026-11-08T09:59:00.000' }), expect.any(Function)));
    expect(within(screen.getByLabelText('host-a log pane')).queryByLabelText('Earlier times')).not.toBeInTheDocument();
    expect(within(screen.getByLabelText('host-b log pane')).getByLabelText('Earlier times')).toBeInTheDocument();
  });
  it('shows merged timeline positions and the same host colors across the viewer', async () => {
    const rows = [
      { ...page.rows[0], ordinal: 2900, mergedOrdinal: 10001, text: 'host a record' },
      { ...page.rows[1], ordinal: 34, mergedOrdinal: 10002, text: 'host b record' },
    ];
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, rows, offset: 10000, total: 12000, pageTotal: 12000 });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('host a record');
    const hostA = screen.getByRole('checkbox', { name: 'Include host host-a' }).closest('label')!;
    const hostB = screen.getByRole('checkbox', { name: 'Include host host-b' }).closest('label')!;
    const colorA = hostA.querySelector('.host-alias')!.getAttribute('style');
    const colorB = hostB.querySelector('.host-alias')!.getAttribute('style');
    expect(colorA).not.toEqual(colorB);
    expect(hostA.querySelector('.host-selector-name')).toHaveAttribute('style', colorA);
    expect(screen.getByLabelText('host-a activity').getAttribute('style')).toContain('--host-color:');
    await userEvent.click(screen.getByRole('button', { name: 'Merge error and access logs' }));
    await screen.findByText('Merged timeline');
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ merged: true }), expect.any(Function)));
    const requests = vi.mocked(logsApi.page).mock.calls.length;
    const recordBody = screen.getByText('host a record');
    expect(screen.getByRole('button', { name: 'Cycle line numbers' })).toHaveTextContent('Off');
    fireEvent.keyDown(window, { key: '#' });
    expect(screen.getByLabelText('File line 2900').closest('.log-viewer')).toHaveClass('number-mode-source');
    expect(screen.getByRole('button', { name: 'Cycle line numbers' })).toHaveTextContent('File');
    fireEvent.keyDown(window, { key: '#' });
    const numberA = screen.getByLabelText('Merged line 10001');
    const numberB = screen.getByLabelText('Merged line 10002');
    expect(numberA.closest('.log-viewer')).toHaveClass('number-mode-overall');
    expect(screen.getByRole('button', { name: 'Cycle line numbers' })).toHaveTextContent('Overall');
    expect(numberA).toHaveAttribute('title', expect.stringContaining('source record 2900'));
    expect(numberB).toHaveAttribute('title', expect.stringContaining('source record 34'));
    expect(numberA.parentElement!.querySelector('.record-host')).toHaveAttribute('style', colorA);
    expect(numberB.parentElement!.querySelector('.record-host')).toHaveAttribute('style', colorB);
    fireEvent.keyDown(window, { key: '#' });
    expect(numberA.closest('.log-viewer')).toHaveClass('number-mode-off');
    expect(screen.getByRole('button', { name: 'Cycle line numbers' })).toHaveTextContent('Off');
    expect(screen.getByText('host a record')).toBe(recordBody);
    expect(logsApi.page).toHaveBeenCalledTimes(requests);
  });

  it('advances by the records actually returned on a size-limited merged page', async () => {
    const rows = Array.from({ length: 5 }, (_, index) => ({ ...page.rows[0], id: `large:${index}`,
      mergedOrdinal: index + 1, text: `large record ${index + 1}` }));
    vi.mocked(logsApi.page).mockImplementation(async (_session, query) => ({ ...page,
      rows: rows.slice(query.offset, query.offset + 3), offset: query.offset, total: 5, pageTotal: 5,
      pageSize: 200, hosts: [{ host: 'host-a', count: 5 }] }));
    render(<LogsWorkspace {...props} />);
    await screen.findByText('large record 1');
    await userEvent.click(screen.getByRole('button', { name: 'Merge error and access logs' }));
    await screen.findByText('Merged timeline');
    fireEvent.keyDown(window, { key: '#' });
    fireEvent.keyDown(window, { key: '#' });
    expect(screen.getByLabelText('Merged line 3')).toBeInTheDocument();
    const next = screen.getByRole('button', { name: 'Next log page' });
    await waitFor(() => expect(next).toBeEnabled());
    await userEvent.click(next);
    await screen.findByLabelText('Merged line 4');
    expect(screen.getByLabelText('Merged line 5')).toBeInTheDocument();
    expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ offset: 3, merged: true }), expect.any(Function));
  });
  it('preserves formatter colors in merged records, including the selected line', async () => {
    const colored = { ...page.rows[0], text: 'ERR 7997 500 /path', lines: [[
      { text: 'ERR', color: '#ff9898' }, { text: ' ', color: null },
      { text: '7997', color: '#e5c07b' }, { text: ' ', color: null },
      { text: '500', color: '#e06c75' }, { text: ' /path', color: '#a0e1e6' },
    ]] };
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, rows: [colored] });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('ERR');
    await userEvent.click(screen.getByRole('button', { name: 'Merge error and access logs' }));
    await screen.findByText('Merged timeline');
    await userEvent.click(screen.getByText('ERR'));
    expect(screen.getByText('ERR')).toHaveStyle({ color: '#ff9898' });
    expect(screen.getByText('ERR')).toHaveClass('log-color-ff9898');
    expect(screen.getByText('7997')).toHaveStyle({ color: '#e5c07b' });
    expect(screen.getByText('500')).toHaveStyle({ color: '#e06c75' });
    expect(screen.getByText('ERR').closest('article')).toHaveAttribute('aria-current', 'true');
  });
  it('retains the selected time after filters leave no records in the window', async () => {
    const rows = [1, 2].map((ordinal) => ({ ...page.rows[0], id: `a:${ordinal}`,
      time: `2026-11-08T10:00:0${ordinal}.000`, text: `selected line ${ordinal}` }));
    vi.mocked(logsApi.page).mockImplementation(async (_session, query) => query.filterRules?.length
      ? { ...page, rows: [], total: 0, hosts: [] }
      : { ...page, rows, hosts: [{ host: 'host-a', count: 2 }] });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('selected line 1');
    await userEvent.click(screen.getByText('selected line 2'));
    await userEvent.click(screen.getByRole('button', { name: 'Filter in (fi)' }));
    fireEvent.change(screen.getByLabelText('Filter in text'), { target: { value: 'no match' } });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Apply' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
    await screen.findByText('No records match this scope.');
    expect(screen.getByLabelText('Current log time')).toHaveTextContent('2026-11-08 10:00:02.000');
    expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      filterRules: [expect.objectContaining({ text: 'no match', action: 'include' })],
    }), expect.any(Function));
  });

  it('Live at the later boundary removes the end limit and follows current remote logs', async () => {
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, availableLast: '2026-11-08T11:00:00.000',
      availableHosts: { 'host-a': { first: page.first, last: '2026-11-08T11:00:00.000' } } });
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Options' }));
    fireEvent.change(screen.getByLabelText('Log end time'), { target: { value: '2026-11-08T10:01:00' } });
    await userEvent.click(screen.getByRole('button', { name: 'Close log options' }));
    await userEvent.click(screen.getByRole('button', { name: 'Live' }));
    expect(screen.getByRole('checkbox', { name: 'Live' })).toBeChecked();
    expect(screen.getByRole('checkbox', { name: 'Follow' })).toBeChecked();
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ end: null, newest: true }), expect.any(Function)));
  });
  it('expands and undoes a time window even when no records match', async () => {
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, rows: [], total: 0, hosts: [], first: null, last: null,
      availableFirst: '2026-11-08T08:00:00.000', availableLast: '2026-11-08T14:00:00.000' });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('No records match this scope.');
    await userEvent.click(screen.getByRole('button', { name: 'Options' }));
    fireEvent.change(screen.getByLabelText('Log start time'), { target: { value: '2026-11-08T10:00:00' } });
    fireEvent.change(screen.getByLabelText('Log end time'), { target: { value: '2026-11-08T10:10:00' } });
    await userEvent.click(screen.getByRole('button', { name: 'Close log options' }));
    await waitFor(() => expect(screen.getByLabelText('Current log time')).toHaveTextContent('2026-11-08 10:05:00.000'));
    await waitFor(() => expect(screen.getAllByRole('button', { name: /Extend before by/ })).toHaveLength(8));
    expect(screen.getAllByRole('button', { name: /Extend after by/ })).toHaveLength(8);
    await userEvent.click(screen.getByRole('button', { name: 'Extend before by 2 minutes' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T09:58:00.000', end: '2026-11-08T10:10:00.000', files: ['a', 'b'],
    }), expect.any(Function)));
    await userEvent.click(screen.getByRole('button', { name: 'Extend after by 100 minutes' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T09:58:00.000', end: '2026-11-08T11:50:00.000',
    }), expect.any(Function)));
    fireEvent.keyDown(window, { key: 'U' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ end: '2026-11-08T10:10:00.000' }), expect.any(Function)));
    await userEvent.click(screen.getByRole('button', { name: 'Extend before by all' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ start: null }), expect.any(Function)));
    await userEvent.click(screen.getByRole('button', { name: 'Extend after by all' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ start: null, end: null }), expect.any(Function)));
  });
  it('checks connection health immediately and every five minutes', async () => {
    vi.useRealTimers();
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-11-08T11:00:00Z'));
    try {
      render(<LogsWorkspace {...props} />);
      await act(async () => { await Promise.resolve(); });
      expect(logsApi.health).toHaveBeenCalledTimes(1);
      await act(async () => { await vi.advanceTimersByTimeAsync(300_000); });
      expect(logsApi.health).toHaveBeenCalledTimes(2);
    } finally { vi.useRealTimers(); }
  });
  it('reports an unreachable server while retaining cached logs', async () => {
    vi.mocked(logsApi.health).mockRejectedValueOnce(new Error('offline'));
    await ready();
    expect(await screen.findByLabelText('Server connection: disconnected')).toBeInTheDocument();
    expect(screen.getByText('message from host-a')).toBeInTheDocument();
  });

  it('hides earlier records immediately when t is pressed while the query is pending', async () => {
    const rows = [1, 2, 3].map((ordinal) => ({ ...page.rows[0], id: `a:${ordinal}`,
      time: `2026-11-08T10:00:0${ordinal}.000`, text: `window line ${ordinal}` }));
    vi.mocked(logsApi.page).mockResolvedValueOnce({ ...page, rows });
    let finish!: (value: LogPage) => void;
    vi.mocked(logsApi.page).mockReturnValueOnce(new Promise((resolve) => { finish = resolve; }));
    render(<LogsWorkspace {...props} />);
    await screen.findByText('window line 2');
    await userEvent.click(screen.getByText('window line 2'));
    fireEvent.keyDown(window, { key: 't' });
    expect(screen.queryByText('window line 1')).not.toBeInTheDocument();
    expect(screen.getByText('window line 2')).toBeInTheDocument();
    await waitFor(() => expect(logsApi.page).toHaveBeenCalledTimes(2));
    finish({ ...page, rows: rows.slice(1) });
  });
  it('opens cached multi-host records without remote discovery or downloads', async () => {
    await ready();
    expect(screen.getByText('message from host-b')).toBeInTheDocument();
    expect(logsApi.discover).not.toHaveBeenCalled();
    expect(logsApi.download).not.toHaveBeenCalled();
    expect(logsApi.page).toHaveBeenCalledWith(1, expect.objectContaining({ files: ['a', 'b'], hosts: [] }), expect.any(Function));
  });

  it('passes local text, host, time and context filters to the shared backend', async () => {
    await ready();
    fireEvent.change(screen.getByLabelText('Search log content'), { target: { value: '/failure/i' } });
    await userEvent.click(screen.getByRole('button', { name: 'Options' }));
    await userEvent.selectOptions(screen.getByLabelText('Context records'), '2');
    await userEvent.click(screen.getByRole('checkbox', { name: 'Include host host-b' }));
    fireEvent.change(screen.getByLabelText('Log start time'), { target: { value: '2026-11-08T09:30' } });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      files: ['a'], hosts: ['host-a'], search: '/failure/i', contextBefore: 2, contextAfter: 2, start: '2026-11-08T09:30:00.000',
    }), expect.any(Function)));
  });

  it('marks automatic live downloads separately from manual downloads', async () => {
    await ready();
    await userEvent.click(screen.getByRole('checkbox', { name: 'Live' }));
    await waitFor(() => expect(logsApi.download).toHaveBeenCalledWith(1, ['a', 'b'], false, expect.any(Function), true));
  });

  it('downloads all selected host copies and allows cancelling a pending transfer', async () => {
    let finish!: (value: LogView) => void;
    vi.mocked(logsApi.download).mockReturnValue(new Promise((resolve) => { finish = resolve; }));
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Download selected logs' }));
    expect(logsApi.download).toHaveBeenCalledWith(1, ['a', 'b'], true, expect.any(Function), false);
    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(logsApi.cancel).toHaveBeenCalledWith(1);
    expect(screen.getByLabelText('Search log content')).toBeEnabled();
    finish(view);
    await waitFor(() => expect(screen.getByRole('button', { name: 'Download selected logs' })).toBeEnabled());
  });

  it('discovers a cached selection automatically before downloading with d', async () => {
    vi.mocked(logsApi.open).mockResolvedValueOnce({ ...view, files: view.files.map((file) => ({ ...file, remote: false })) });
    await ready();
    expect(screen.getByRole('button', { name: 'Download selected logs' })).toBeEnabled();
    fireEvent.keyDown(screen.getByRole('button', { name: /ROOT Error 2026-11-08:/ }), { key: 'd' });
    await waitFor(() => expect(logsApi.download).toHaveBeenCalledWith(1, ['a', 'b'], true, expect.any(Function), false));
    expect(logsApi.discover).toHaveBeenCalledOnce();
    fireEvent.keyDown(screen.getByLabelText('Search log content'), { key: 'd' });
    expect(logsApi.download).toHaveBeenCalledOnce();
  });

  it('queues downloads for different cells and marks the active and queued cells', async () => {
    const access = { ...view.files[0], id: 'access', kind: 'ACC', name: 'AccessLog.txt' };
    const all = { ...view, files: [...view.files, access] };
    vi.mocked(logsApi.open).mockResolvedValue(all);
    let finishFirst!: (value: LogView) => void, finishSecond!: (value: LogView) => void;
    vi.mocked(logsApi.download).mockReturnValueOnce(new Promise((resolve) => { finishFirst = resolve; }))
      .mockReturnValueOnce(new Promise((resolve) => { finishSecond = resolve; }));
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Download selected logs' }));
    expect(await screen.findByRole('button', { name: /ROOT Error 2026-11-08:.*download active/ })).toHaveTextContent('↓');
    await userEvent.click(screen.getByRole('button', { name: /ROOT Access 2026-11-08:/ }));
    await userEvent.click(screen.getByRole('button', { name: 'Download selected logs' }));
    expect(screen.getByRole('button', { name: /ROOT Access 2026-11-08:.*download queued/ })).toHaveTextContent('Q');
    expect(logsApi.download).toHaveBeenCalledTimes(1);
    finishFirst(all);
    await waitFor(() => expect(logsApi.download).toHaveBeenCalledTimes(2));
    expect(logsApi.download).toHaveBeenLastCalledWith(1, ['access'], true, expect.any(Function), false);
    expect(screen.getByRole('button', { name: /ROOT Access 2026-11-08:.*download active/ })).toBeInTheDocument();
    finishSecond(all);
    await waitFor(() => expect(screen.queryByRole('button', { name: /download active/ })).not.toBeInTheDocument());
  });

  it('cancels the active download and removes queued requests', async () => {
    const access = { ...view.files[0], id: 'access', kind: 'ACC', name: 'AccessLog.txt' };
    const all = { ...view, files: [...view.files, access] };
    vi.mocked(logsApi.open).mockResolvedValue(all);
    let finish!: (value: LogView) => void;
    vi.mocked(logsApi.download).mockReturnValueOnce(new Promise((resolve) => { finish = resolve; }));
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Download selected logs' }));
    await userEvent.click(screen.getByRole('button', { name: /ROOT Access 2026-11-08:/ }));
    await userEvent.click(screen.getByRole('button', { name: 'Download selected logs' }));
    expect(screen.getByRole('button', { name: /download queued/ })).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(logsApi.cancel).toHaveBeenCalledWith(1);
    expect(screen.queryByRole('button', { name: /download queued/ })).not.toBeInTheDocument();
    finish(all);
    await waitFor(() => expect(screen.queryByRole('button', { name: 'Cancel' })).not.toBeInTheDocument());
    expect(logsApi.download).toHaveBeenCalledTimes(1);
  });

  it('explains when discovery finds no selected remote file', async () => {
    const local = { ...view, files: view.files.map((file) => ({ ...file, remote: false })) };
    vi.mocked(logsApi.open).mockResolvedValueOnce(local);
    vi.mocked(logsApi.discover).mockResolvedValueOnce(local);
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Download selected logs' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('No remote files found');
    expect(logsApi.download).not.toHaveBeenCalled();
  });

  it('merges host records and expands the exact original record', async () => {
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Merge error and access logs' }));
    await screen.findByText('Merged timeline');
    fireEvent.keyDown(window, { key: 'v' });
    expect(screen.getByLabelText('Log options')).toBeInTheDocument();
    fireEvent.keyDown(window, { key: 'v' });
    expect(screen.queryByLabelText('Log options')).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: /Inspect host-a/ }));
    expect(screen.getByText('raw from host-a')).toBeInTheDocument();
    expect(screen.queryByText('raw from host-b')).not.toBeInTheDocument();
  });

  it('keeps the expanded record open during live refresh, pauses follow and copies the formatted detail', async () => {
    const raw = '2026-11-08 10:00:00.000 Critical: {"foo":"bar"}';
    const refreshed = { ...page, rows: [{ ...page.rows[0], raw, level: 'Critical' }, page.rows[1]] };
    vi.mocked(logsApi.page).mockResolvedValue(refreshed);
    const copy = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText: copy }, configurable: true });
    await ready();
    await userEvent.click(screen.getByRole('checkbox', { name: 'Live' }));
    expect(screen.getByRole('checkbox', { name: 'Follow' })).toBeChecked();
    await userEvent.click(screen.getByRole('button', { name: /Inspect host-a/ }));
    expect(screen.getByRole('checkbox', { name: 'Follow' })).not.toBeChecked();
    expect(screen.getByRole('checkbox', { name: 'Live' })).toBeChecked();
    await waitFor(() => expect(vi.mocked(logsApi.page).mock.calls.length).toBeGreaterThan(1));
    expect(screen.getByText(/"foo": "bar"/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Copy record from host-a' }));
    expect(copy).toHaveBeenCalledWith('2026-11-08 10:00:00.000 Critical: {\n  "foo": "bar"\n}');
    expect(await screen.findByText('Copied')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: /Inspect host-a/ }));
    expect(screen.queryByText(/"foo": "bar"/)).not.toBeInTheDocument();
  });

  it('selects a timeline bucket with the keyboard and can reset the window', async () => {
    await ready();
    // detail=0 represents keyboard activation, which must work without pointer events.
    fireEvent.click(screen.getByRole('button', { name: 'host-a activity 2026-11-08T10:00:00.000: 2 records' }), { detail: 0 });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T10:00:00.000', end: '2026-11-08T10:00:00.000',
    }), expect.any(Function)));
    await userEvent.click(screen.getByRole('button', { name: 'Options' }));
    await userEvent.click(screen.getByRole('button', { name: 'Reset time' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ start: null, end: null }), expect.any(Function)));
  });

  it('applies and undoes a time window using the log line menu', async () => {
    await ready();
    fireEvent.contextMenu(screen.getByText('message from host-b'), { clientX: 100, clientY: 100 });
    expect(screen.getByRole('menu', { name: 'Log line actions' })).toBeInTheDocument();
    await userEvent.click(screen.getByRole('menuitem', { name: /Hide after this line/ }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: null, end: '2026-11-08T10:00:00.000',
    }), expect.any(Function)));
    fireEvent.contextMenu(screen.getByText('message from host-b'), { clientX: 100, clientY: 100 });
    await userEvent.click(screen.getByRole('menuitem', { name: /Undo window change/ }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ start: null, end: null }), expect.any(Function)));
  });

  it('navigates host records with j and k and opens and closes the selected detail with v', async () => {
    const rows = ['00', '01', '02'].map((second) => ({ ...page.rows[0], id: `a:${second}`,
      time: `2026-11-08T10:00:${second}.000`, text: `entry ${second}`, raw: `original ${second}` }));
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, rows });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('entry 00');
    await userEvent.click(screen.getByText('entry 00'));
    fireEvent.keyDown(window, { key: 'j' });
    expect(screen.getByText('entry 01').closest('article')).toHaveClass('selected-record');
    fireEvent.keyDown(window, { key: 'v' });
    expect(screen.getByText('original 01')).toBeInTheDocument();
    fireEvent.keyDown(window, { key: 'v' });
    expect(screen.queryByText('original 01')).not.toBeInTheDocument();
    fireEvent.keyDown(window, { key: 'k' });
    expect(screen.getByText('entry 00').closest('article')).toHaveClass('selected-record');
  });

  it('uses the focused host pane for keyboard navigation and toggles the number gutter with #', async () => {
    const rows = [1, 2, 3].map((ordinal) => ({ ...page.rows[0], id: `a:${ordinal}`, ordinal,
      time: `2026-11-08T10:00:0${ordinal}.000`, text: `line ${ordinal}` }));
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, rows });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('line 1');
    const pane = screen.getByLabelText('Read host-a logs');
    fireEvent.focus(pane);
    fireEvent.keyDown(pane, { key: 'j' });
    expect(screen.getByText('line 2').closest('article')).toHaveAttribute('aria-current', 'true');
    fireEvent.keyDown(pane, { key: '#' });
    expect(screen.getByLabelText('File line 2').closest('.log-viewer')).toHaveClass('number-mode-source');
    fireEvent.keyDown(pane, { key: 'G' });
    expect(screen.getByText('line 3').closest('article')).toHaveAttribute('aria-current', 'true');
    fireEvent.keyDown(pane, { key: 'g' });
    expect(screen.getByText('line 1').closest('article')).toHaveAttribute('aria-current', 'true');
    fireEvent.keyDown(pane, { key: '#' });
    expect(screen.getByLabelText('Host line 2').closest('.log-viewer')).toHaveClass('number-mode-overall');
    fireEvent.keyDown(pane, { key: '#' });
    expect(screen.getByLabelText('File line 2').closest('.log-viewer')).toHaveClass('number-mode-off');
    expect(logsApi.page).toHaveBeenCalledOnce();
  });

  it('edits filter rules with fi/fo, restores on Escape and clears with fc', async () => {
    await ready();
    await userEvent.click(screen.getByText('message from host-a'));
    fireEvent.keyDown(window, { key: 'f' }); fireEvent.keyDown(window, { key: 'i' });
    const include = screen.getByRole('textbox', { name: 'Filter in text' });
    expect(include).toHaveFocus();
    fireEvent.change(include, { target: { value: 'error\nTimeout' } });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Apply' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ filterRules: [expect.objectContaining({text:'error'}), expect.objectContaining({text:'Timeout'})] }), expect.any(Function)));
    fireEvent.keyDown(window, { key: 'f' }); fireEvent.keyDown(window, { key: 'o' });
    const exclude = screen.getByRole('textbox', { name: 'Filter out text' });
    expect(exclude).toHaveFocus();
    fireEvent.change(exclude, { target: { value: 'noise' } });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Add terms' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Add terms' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ filterRules: expect.arrayContaining([expect.objectContaining({text:'noise',action:'exclude'})]) }), expect.any(Function)));
    fireEvent.keyDown(exclude, { key: 'Escape' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ filterRules: [expect.objectContaining({text:'error'}), expect.objectContaining({text:'Timeout'})] }), expect.any(Function)));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Search log content'), { target: { value: 'search' } });
    fireEvent.keyDown(window, { key: 'f' }); fireEvent.keyDown(window, { key: 'c' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ filterRules: [], search: '' }), expect.any(Function)));
  });

  it('exposes filter buttons and Ctrl+U clears an edited filter', async () => {
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Filter out (fo)' }));
    const input = screen.getByRole('textbox', { name: 'Filter out text' });
    fireEvent.change(input, { target: { value: 'noise' } });
    fireEvent.keyDown(input, { key: 'u', ctrlKey: true });
    expect(input).toHaveValue('');
    fireEvent.change(input, { target: { value: 'debug' } });
    await waitFor(() => expect(screen.getByRole('button', { name: /Apply/ })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: /Apply/ }));
    expect(screen.getByRole('button', { name: 'Filter out (fo)' })).toHaveAttribute('aria-pressed', 'true');
    await userEvent.click(screen.getByRole('button', { name: 'Clear content filters (fc)' }));
    expect(screen.getByRole('button', { name: 'Filter out (fo)' })).toHaveAttribute('aria-pressed', 'false');
  });

  it('includes selected log text literally and drops only the chosen field', async () => {
    await ready();
    const log = screen.getByText('message from host-a');
    const selection = window.getSelection()!;
    const range = document.createRange();
    range.selectNodeContents(log); selection.removeAllRanges(); selection.addRange(range);
    fireEvent.contextMenu(log, { clientX: 100, clientY: 100 });
    expect(screen.getByRole('menuitem', { name: 'Include selected text' })).toBeInTheDocument();
    await userEvent.click(screen.getByRole('menuitem', { name: 'Include selected text' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ filterRules: [expect.objectContaining({ text: 'message from host-a', mode: 'literal', action: 'include', field: 'any', case: 'sensitive' })] }), expect.any(Function)));
    selection.removeAllRanges();
    fireEvent.contextMenu(log, { clientX: 100, clientY: 100 });
    await userEvent.click(screen.getByRole('menuitem', { name: 'Drop port ----' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ filterRules: expect.arrayContaining([expect.objectContaining({ text: '----', mode: 'exact', action: 'exclude', field: 'port' })]) }), expect.any(Function)));
  });

  it('retains the TUI delayed standalone f command for follow', async () => {
    await ready();
    await userEvent.click(screen.getByText('message from host-a'));
    fireEvent.keyDown(window, { key: 'f' });
    expect(screen.getByRole('checkbox', { name: 'Follow' })).not.toBeChecked();
    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Follow' })).toBeChecked(), { timeout: 1500 });
  });

  it('G stops following without replacing the displayed tail of a quiet host', async () => {
    vi.mocked(logsApi.page).mockImplementation(async (_session, query) => ({ ...page, offset: query.newest ? 900 : 0,
      pageSize: 100, pageTotal: 1000, hosts: [{ host: 'host-a', count: 1 }, { host: 'host-b', count: 1000 }] }));
    await ready();
    await userEvent.click(screen.getByRole('checkbox', { name: 'Follow' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ newest: true }), expect.any(Function)));
    await userEvent.click(screen.getByText('message from host-a'));
    const requests = vi.mocked(logsApi.page).mock.calls.length;
    await userEvent.keyboard('G');
    expect(screen.getByRole('checkbox', { name: 'Follow' })).not.toBeChecked();
    expect(screen.getByText('message from host-a').closest('article')).toHaveAttribute('aria-current', 'true');
    expect(logsApi.page).toHaveBeenCalledTimes(requests);
  });

  it.each([false, true])('g/G jump across pages within the filtered time window (merged=%s)', async (merged) => {
    const rows = Array.from({ length: 450 }, (_, index) => ({ ...page.rows[0], id: `a:${index}`,
      time: `2026-11-08T10:${String(Math.floor(index / 60)).padStart(2, '0')}:${String(index % 60).padStart(2, '0')}.000`, text: `window entry ${index}` }));
    vi.mocked(logsApi.page).mockImplementation(async (_session, query) => {
      const size = query.splitHosts ? 100 : 200;
      const offset = query.newest ? rows.length - size : query.offset;
      return { ...page, rows: rows.slice(offset, offset + size), offset, total: rows.length, pageTotal: rows.length,
        pageSize: size, hosts: [{ host: 'host-a', count: rows.length }] };
    });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('window entry 0');
    if (merged) await userEvent.click(screen.getByRole('button', { name: 'Merge error and access logs' }));
    await userEvent.click(screen.getByRole('button', { name: 'Options' }));
    await userEvent.click(screen.getByRole('button', { name: /Manage keep terms/ }));
    fireEvent.change(screen.getByLabelText('Filter in text'), { target: { value: 'window' } });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Apply' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
    await userEvent.click(screen.getByRole('button', { name: 'Options' }));
    fireEvent.change(screen.getByLabelText('Log start time'), { target: { value: '2026-11-08T10:00:00' } });
    fireEvent.change(screen.getByLabelText('Log end time'), { target: { value: '2026-11-08T10:07:29' } });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ filterRules: [expect.objectContaining({ text: 'window', action: 'include' })], end: '2026-11-08T10:07:29.000' }), expect.any(Function)));
    await userEvent.click(screen.getByRole('button', { name: 'Close log options' }));
    await userEvent.click(screen.getByText('window entry 0'));
    await userEvent.keyboard('G');
    await waitFor(() => expect(screen.getByText('window entry 449').closest('article')).toHaveAttribute('aria-current', 'true'));
    expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ newest: true, filterRules: [expect.objectContaining({ text: 'window', action: 'include' })], start: '2026-11-08T10:00:00.000', end: '2026-11-08T10:07:29.000' }), expect.any(Function));
    await userEvent.keyboard('gg');
    await waitFor(() => expect(screen.getByText('window entry 0').closest('article')).toHaveAttribute('aria-current', 'true'));
    expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ newest: false, offset: 0, filterRules: [expect.objectContaining({ text: 'window', action: 'include' })] }), expect.any(Function));
  });

  it('keeps the current log visible while a changed filter is evaluated', async () => {
    await ready();
    let finish!: (value: LogPage) => void;
    vi.mocked(logsApi.page).mockReturnValueOnce(new Promise((resolve) => { finish = resolve; }));
    fireEvent.change(screen.getByLabelText('Search log content'), { target: { value: 'new filter' } });
    await waitFor(() => expect(logsApi.page).toHaveBeenCalledTimes(2));
    expect(screen.getByText('message from host-a')).toBeInTheDocument();
    expect(screen.queryByText('Reading log cache…')).not.toBeInTheDocument();
    finish({ ...page, rows: [{ ...page.rows[0], text: 'filtered entry' }] });
    await screen.findByText('filtered entry');
  });

  it('G targets the filtered window end when a filter request is still pending', async () => {
    await ready();
    let finish!: (value: LogPage) => void;
    vi.mocked(logsApi.page).mockReturnValueOnce(new Promise((resolve) => { finish = resolve; }))
      .mockResolvedValueOnce({ ...page, rows: [{ ...page.rows[0], id: 'end', text: 'filtered last' }],
        total: 500, pageTotal: 500, offset: 400, pageSize: 100, hosts: [{ host: 'host-a', count: 500 }] });
    fireEvent.change(screen.getByLabelText('Search log content'), { target: { value: 'filtered' } });
    await waitFor(() => expect(logsApi.page).toHaveBeenCalledTimes(2));
    await userEvent.click(screen.getByText('message from host-a'));
    await userEvent.keyboard('G');
    await waitFor(() => expect(screen.getByText('filtered last').closest('article')).toHaveAttribute('aria-current', 'true'));
    expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ newest: true, search: 'filtered' }), expect.any(Function));
    finish(page);
  });

  it('clears a pending edge jump when the filtered window has no results', async () => {
    const rows = [1, 2].map((ordinal) => ({ ...page.rows[0], id: `a:${ordinal}`, text: `retained entry ${ordinal}` }));
    vi.mocked(logsApi.page).mockImplementation(async (_session, query) => {
      if (query.search && !query.newest) return new Promise<LogPage>(() => {});
      if (query.search) return { ...page, rows: [], total: 0, pageTotal: 0, hosts: [] };
      return { ...page, rows, hosts: [{ host: 'host-a', count: 2 }] };
    });
    render(<LogsWorkspace {...props} />);
    await screen.findByText('retained entry 1');
    await userEvent.click(screen.getByText('retained entry 1'));
    fireEvent.change(screen.getByLabelText('Search log content'), { target: { value: 'nothing' } });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({ search: 'nothing', newest: false }), expect.any(Function)));
    await userEvent.keyboard('G');
    await screen.findByText('No records match this scope.');
    fireEvent.change(screen.getByLabelText('Search log content'), { target: { value: '' } });
    await waitFor(() => expect(screen.getByText('retained entry 1').closest('article')).toHaveAttribute('aria-current', 'true'));
  });

  it('sets, centers and undoes a time window around the selected log entry', async () => {
    await ready();
    await userEvent.click(screen.getByText('message from host-a'));
    fireEvent.keyDown(window, { key: 't' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T10:00:00.000', end: null,
    }), expect.any(Function)), { timeout: 2000 });
    await new Promise((resolve) => setTimeout(resolve, 750));
    fireEvent.keyDown(window, { key: 't' });
    fireEvent.keyDown(window, { key: 't' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T09:57:30.000', end: '2026-11-08T10:02:30.000',
    }), expect.any(Function)));
    fireEvent.keyDown(window, { key: 'U' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T10:00:00.000', end: null,
    }), expect.any(Function)));
  });

  it('moves into the next cached page and keeps the window when changing grid cells', async () => {
    const rows = ['00', '01', '02', '03'].map((second) => ({ ...page.rows[0], id: `a:${second}`,
      time: `2026-11-08T10:00:${second}.000`, text: `entry ${second}` }));
    vi.mocked(logsApi.page).mockImplementation(async (_session, query) => ({ ...page,
      rows: rows.slice(query.offset, query.offset + 2), hosts: [{ host: 'host-a', count: 4 }],
      total: 4, pageTotal: 4, pageSize: 2, offset: query.offset }));
    render(<LogsWorkspace {...props} />);
    await screen.findByText('entry 00');
    await userEvent.click(screen.getByText('entry 00'));
    fireEvent.keyDown(window, { key: 'j' });
    fireEvent.keyDown(window, { key: 'j' });
    await waitFor(() => expect(screen.getByText('entry 02').closest('article')).toHaveClass('selected-record'));
    fireEvent.keyDown(window, { key: 't' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T10:00:02.000',
    }), expect.any(Function)), { timeout: 2000 });
    await userEvent.click(screen.getByRole('button', { name: 'Browse files' }));
    await userEvent.click(screen.getByRole('button', { name: /ROOT Error 2026-11-07:/ }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      start: '2026-11-08T10:00:02.000',
    }), expect.any(Function)));
  });

  it('keeps seven days visible while reading and can hide or restore the grid', async () => {
    await ready();
    const area = screen.getByTestId('log-work-area');
    expect(area).toHaveClass('grid-expanded');
    expect(screen.getByRole('table')).toHaveStyle({ width: '496px' });
    await userEvent.click(screen.getByText('message from host-a'));
    expect(area).toHaveClass('grid-expanded');
    expect(screen.getByRole('table')).toHaveStyle({ width: '496px' });
    expect(screen.getByRole('button', { name: 'Select Error column 2026-11-08' })).toHaveTextContent('E');
    expect(screen.getByRole('button', { name: 'Select Error column 2026-11-07' })).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Hide files' }));
    expect(area).toHaveClass('grid-hidden');
    expect(screen.queryByRole('table')).not.toBeInTheDocument();
    fireEvent.keyDown(window, { key: '\\' });
    expect(area).toHaveClass('grid-expanded');
  });

  it('merges error and access for the selected port without including request logs', async () => {
    const extraFiles = ['ACC', 'REQ'].map((kind) => ({ ...view.files[0], id: kind, kind,
      name: kind === 'ACC' ? 'AccessLog.txt' : 'RequestLog.txt' }));
    vi.mocked(logsApi.open).mockResolvedValueOnce({ ...view, files: [...view.files, ...extraFiles] });
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Merge error and access logs' }));
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      files: ['ACC', 'a', 'b'], merged: true, splitHosts: false,
    }), expect.any(Function)));
    expect(screen.getByTestId('log-work-area')).toHaveClass('grid-expanded');
    fireEvent.keyDown(window, { key: 'm' });
    await waitFor(() => expect(logsApi.page).toHaveBeenLastCalledWith(1, expect.objectContaining({
      files: ['a', 'b'], merged: false, splitHosts: true,
    }), expect.any(Function)));
  });

  it('ignores a late page from a previous connection' , async () => {
    let finishOld!: (value: LogPage) => void;
    vi.mocked(logsApi.page).mockReturnValueOnce(new Promise((resolve) => { finishOld = resolve; }));
    const rendered = render(<LogsWorkspace {...props} />);
    await waitFor(() => expect(logsApi.page).toHaveBeenCalledOnce());
    vi.mocked(logsApi.open).mockResolvedValueOnce({ ...view, session: 2 });
    vi.mocked(logsApi.page).mockResolvedValue({ ...page, rows: [{ ...page.rows[0], text: 'new connection record' }] });
    rendered.rerender(<LogsWorkspace {...props} connection={{ ...props.connection, server: 'other' }} />);
    await screen.findByText('new connection record');
    finishOld(page);
    await waitFor(() => expect(screen.queryByText('message from host-a')).not.toBeInTheDocument());
  });
});

it('embeds a merged port tail with a ten-second refresh and retains log tools', async () => {
 const files = [
  {...view.files[0],id:'port-error',port:'9060',name:'9060_ErrorLog.txt'},
  {...view.files[1],id:'port-access',port:'9060',kind:'ACC',name:'9060_AccessLog.txt'},
  view.files[0],
 ];
 const portView={...view,files};
 vi.mocked(logsApi.open).mockResolvedValue(portView);
 vi.mocked(logsApi.discover).mockResolvedValue(portView);
 vi.mocked(logsApi.download).mockResolvedValue(portView);
 const interval=vi.spyOn(globalThis,'setInterval');
 const now=Date.now();
 const clock=vi.spyOn(Date,'now').mockReturnValue(now);
 const element=render(<LogsWorkspace {...props} embedded preferredPort="9060" refreshInterval={10000}/>);
 await waitFor(()=>expect(logsApi.download).toHaveBeenCalledWith(1,['port-error','port-access'],false,expect.any(Function),true));
 expect(screen.getByRole('combobox',{name:'Query log port'})).toHaveValue('9060');
 expect(screen.getByRole('checkbox',{name:'Live (10s)'})).toBeChecked();
 expect(screen.getByRole('checkbox',{name:'Follow'})).toBeChecked();
 expect(screen.getByLabelText(/Next log refresh in [0-9]+ seconds/)).toBeInTheDocument();
 const countdown=interval.mock.calls.find(call=>call[1]===250)?.[0];
 expect(typeof countdown).toBe('function');
 clock.mockReturnValue(now+3000);
 act(()=>{if(typeof countdown==='function')countdown();});
 expect(screen.getByLabelText('Next log refresh in 7 seconds')).toBeInTheDocument();
 clock.mockRestore();
 await waitFor(()=>expect(logsApi.page).toHaveBeenLastCalledWith(1,expect.objectContaining({merged:true,newest:true,files:['port-access','port-error']}),expect.any(Function)));
 expect(screen.getByRole('button',{name:'Filter in (fi)'})).toBeInTheDocument();
 const tick=interval.mock.calls.find(call=>call[1]===10000)?.[0];
 expect(typeof tick).toBe('function');
 await waitFor(()=>expect(screen.queryByText('Refreshing log tails')).not.toBeInTheDocument());
 vi.mocked(logsApi.download).mockClear();
 await act(async()=>{if(typeof tick==='function')tick();});
 await waitFor(()=>expect(logsApi.download).toHaveBeenCalledTimes(1));
 element.rerender(<LogsWorkspace {...props} active={false} embedded preferredPort="9060" refreshInterval={10000}/>);
 element.unmount();
 expect(logsApi.close).toHaveBeenCalledWith(1);
 interval.mockRestore();
});
