import { invoke } from '@tauri-apps/api/core';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, it, vi } from 'vitest';
import { MetersWorkspace } from './MetersWorkspace';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

it('shows each native capture when the host is offline', async () => {
  const connection = { server: 'local', appServer: null, database: 'Documents' };
  const capturedAt = '2026-11-23T09:35:00.000Z';
  const xml = '<host-meters><cpu>15</cpu></host-meters>';
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (!(args as { cached: boolean }).cached) throw { offline: true, message: 'Connection refused' };
    const data = command === 'raw_meters' ? [{ uri: '/host-raw.xml', xml }] : 'Native support status';
    return { capturedAt, data };
  });
  render(<MetersWorkspace active connection={connection}/>);
  expect(await screen.findByText(xml)).toBeVisible();
  expect(await screen.findByText('Offline.')).toBeVisible();
  expect(invoke).toHaveBeenCalledWith('raw_meters', { connection, cached: false, database: 'Meters' });
  expect(invoke).not.toHaveBeenCalledWith('support_dump', expect.anything());

  await userEvent.click(screen.getByRole('button', { name: 'System status dump' }));
  expect(await screen.findByText('Native support status')).toBeVisible();
  expect(await screen.findByText('Offline.')).toBeVisible();
  expect(screen.queryByText(xml)).not.toBeInTheDocument();
});
