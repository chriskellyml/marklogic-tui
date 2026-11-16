import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { LogArchivePanel, defaultArchiveSettings } from './LogArchivePanel';
import { open } from '@tauri-apps/plugin-dialog';
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));

describe('log archive settings', () => {
  it('saves and runs the default seven-day policy with a chosen disk', async () => {
    const onSave = vi.fn().mockResolvedValue(undefined), onRun = vi.fn().mockResolvedValue(undefined);
    render(<LogArchivePanel initial={defaultArchiveSettings} busy={false} message="" onSave={onSave} onRun={onRun} onClose={vi.fn()} />);
    expect(screen.getByRole('button', { name: 'Archive now' })).toBeDisabled();
    fireEvent.change(screen.getByLabelText('Archive folder'), { target: { value: '/Volumes/Archive/logs' } });
    await userEvent.click(screen.getByRole('button', { name: 'Save settings' }));
    expect(onSave).toHaveBeenCalledWith({ folder: '/Volumes/Archive/logs', compressAfterDays: 7, archiveAfterDays: 7 });
    await userEvent.click(screen.getByRole('button', { name: 'Archive now' }));
    expect(onRun).toHaveBeenCalledWith({ folder: '/Volumes/Archive/logs', compressAfterDays: 7, archiveAfterDays: 7 });
  });
  it('persists the selected folder immediately', async () => {
    vi.mocked(open).mockResolvedValue('/Volumes/Remote');
    const onSave = vi.fn().mockResolvedValue(undefined);
    render(<LogArchivePanel initial={defaultArchiveSettings} busy={false} message="" onSave={onSave} onRun={vi.fn()} onClose={vi.fn()} />);
    await userEvent.click(screen.getByRole('button', { name: 'Choose folder…' }));
    await waitFor(() => expect(onSave).toHaveBeenCalledWith({ folder: '/Volumes/Remote', compressAfterDays: 7, archiveAfterDays: 7 }));
    expect(screen.getByLabelText('Archive folder')).toHaveValue('/Volumes/Remote');
  });
  it('supports two-stage archival and rejects reversed retention ages', async () => {
    const onRun = vi.fn().mockResolvedValue(undefined);
    render(<LogArchivePanel initial={{ ...defaultArchiveSettings, folder: '/archive' }} busy={false} message="" onSave={vi.fn()} onRun={onRun} onClose={vi.fn()} />);
    fireEvent.change(screen.getByLabelText('Archive logs older than days'), { target: { value: '6' } });
    expect(screen.getByRole('button', { name: 'Archive now' })).toBeDisabled();
    fireEvent.change(screen.getByLabelText('Archive logs older than days'), { target: { value: '14' } });
    await userEvent.click(screen.getByRole('button', { name: 'Archive now' }));
    expect(onRun).toHaveBeenCalledWith({ folder: '/archive', compressAfterDays: 7, archiveAfterDays: 14 });
  });
  it('shows save failures and disables changes while archiving', async () => {
    const props = { initial: { ...defaultArchiveSettings, folder: '/archive' }, busy: false, message: '', onSave: vi.fn().mockRejectedValue(new Error('Disk unavailable')), onRun: vi.fn(), onClose: vi.fn() };
    const { rerender } = render(<LogArchivePanel {...props} />);
    await userEvent.click(screen.getByRole('button', { name: 'Save settings' }));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent('Disk unavailable'));
    rerender(<LogArchivePanel {...props} busy message="Archive complete" />);
    expect(screen.getByLabelText('Archive folder')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Archive now' })).toBeDisabled();
  });
});
