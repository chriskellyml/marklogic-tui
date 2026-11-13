import { fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { LogGrid } from './LogGrid';
import type { LogFile } from './logs-api';
const today = '2026-11-08';
const files: LogFile[] = ['8080', '8081'].flatMap((port) => ['ERR', 'ACC', 'REQ'].map((kind) => ({ id: `${port}-${kind}`, port, kind, name: `${port}_${kind}Log.txt`, host: 'host-a', days: [today, '2026-11-07'], cached: true, complete: true, cachedBytes: 10, size: 10, remote: false, stale: false, modified: '', updatedAt: null })));
function setup() {
  const onSelect = vi.fn(), onMarks = vi.fn();
  render(<LogGrid files={files} today={today} selection={{ mode: 'cell', port: '8080', kind: 'ERR', day: today }} marks={{}} onSelect={onSelect} onMarks={onMarks} opening={false} />);
  return { onSelect, onMarks };
}
describe('port and day grid navigation', () => {
  it('renders one row per port and Err/Acc/Req columns under each date', () => {
    setup();
    expect(screen.getByRole('table', { name: /MarkLogic logs/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /8080 Error 2026-11-08:/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /8081 Request 2026-11-07:/ })).toBeInTheDocument();
    expect(screen.getAllByRole('row')).toHaveLength(4);
  });
  it('selects a type column across ports and a port row across days', async () => {
    const { onSelect } = setup();
    await userEvent.click(screen.getByRole('button', { name: 'Select Access column 2026-11-07' }));
    expect(onSelect).toHaveBeenLastCalledWith({ mode: 'column', port: '8080', kind: 'ACC', day: '2026-11-07', ports: ['8080', '8081'] });
    await userEvent.click(screen.getByRole('button', { name: 'Select port 8081 Access across all days' }));
    expect(onSelect).toHaveBeenLastCalledWith({ mode: 'row', port: '8081', kind: 'ACC', day: '2026-11-07' });
  });
  it('moves between ports, types and days with the original keyboard actions', async () => {
    const { onSelect } = setup();
    const cell = screen.getByRole('button', { name: /8080 Error 2026-11-08:/ });
    await userEvent.click(cell);
    fireEvent.keyDown(cell, { key: 'ArrowRight' });
    expect(onSelect).toHaveBeenLastCalledWith({ mode: 'cell', port: '8080', kind: 'ACC', day: today });
    fireEvent.keyDown(cell, { key: 'PageDown' });
    expect(onSelect).toHaveBeenLastCalledWith({ mode: 'cell', port: '8080', kind: 'ACC', day: '2026-11-07' });
    fireEvent.keyDown(cell, { key: 'ArrowDown' });
    expect(onSelect).toHaveBeenLastCalledWith({ mode: 'cell', port: '8081', kind: 'ACC', day: '2026-11-07' });
    fireEvent.keyDown(cell, { key: 'V' });
    expect(onSelect).toHaveBeenLastCalledWith({ mode: 'row', port: '8081', kind: 'ACC', day: '2026-11-07' });
    fireEvent.keyDown(cell, { key: 'C' });
    expect(onSelect).toHaveBeenLastCalledWith({ mode: 'column', port: '8081', kind: 'ACC', day: '2026-11-07', ports: ['8080', '8081'] });
  });
  it('can browse older days without downloading anything', async () => {
    setup();
    await userEvent.click(screen.getByRole('button', { name: 'Older log days' }));
    expect(screen.getByLabelText('First grid day')).toHaveValue('2026-11-01');
    expect(screen.getByRole('button', { name: /8080 Error 2026-11-01: no file/ })).toBeInTheDocument();
  });
  it('opens a row selection with Enter without replacing it with a single cell', () => {
    const onSelect = vi.fn(), onOpen = vi.fn();
    render(<LogGrid files={files} today={today} selection={{ mode: 'row', port: '8080', kind: 'ACC', day: today }}
      marks={{}} onSelect={onSelect} onMarks={vi.fn()} onOpen={onOpen} opening={false} />);
    fireEvent.keyDown(screen.getByRole('button', { name: /8080 Access 2026-11-08:/ }), { key: 'Enter' });
    expect(onOpen).toHaveBeenCalledOnce();
    expect(onSelect).not.toHaveBeenCalled();
  });
});
