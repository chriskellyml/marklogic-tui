import { fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import { CorbiRunHistory } from './CorbiRunHistory';
import { corbiApi } from './corbi-api';
import { corbiLogRecords, reportRows } from './corbi-output';

vi.mock('./corbi-api', () => ({ corbiApi: { history: vi.fn(), artifacts: vi.fn(), output: vi.fn() } }));
const jul = 'Nov 14, 2026 1:27:54 PM com.marklogic.developer.corb.AbstractManager logRuntimeArgs\nINFO: runtime arguments = --add-modules=ALL-DEFAULT\nNov 14, 2026 1:27:55 PM com.marklogic.developer.corb.Manager run\nWARNING: Slow request\nSEVERE: Operation failed\n  at some.stack.frame\nINFO: Completed';
beforeEach(() => {
  vi.mocked(corbiApi.history).mockResolvedValue([{ id: '20261114132754', environment: 'dev' }]);
  vi.mocked(corbiApi.artifacts).mockResolvedValue(['dry-output.log', 'dry-report.txt', 'job.options']);
  vi.mocked(corbiApi.output).mockImplementation(async (_root, _project, _run, file) => file === 'dry-output.log' ? jul : file === 'job.options' ? 'THREAD-COUNT=4' : 'uri,status\n/a.xml,ok\n/b.xml,ok');
});

it('defaults to a report and compact tailed log, supports table controls, resizing and deliberate file inspection', async () => {
  render(<CorbiRunHistory root="/data" project="repair" run={null} active initial={null}/>);
  const table = await screen.findByRole('table', { name: 'Run report' });
  expect(within(table).getByText('/a.xml')).toBeInTheDocument();
  expect(screen.queryByLabelText('Run artifact')).toBeNull();
  const log = screen.getByLabelText('Run log');
  expect(log).toHaveTextContent('1:27:54 PM');
  expect(log).not.toHaveTextContent('com.marklogic.developer.corb');
  expect(within(log).getByText('Slow request').closest('.corbi-log-record')).toHaveAttribute('data-tone', 'warning');
  expect(screen.getByLabelText('Follow tail')).toBeChecked();
  expect(corbiApi.output).toHaveBeenCalledWith('/data', 'repair', expect.anything(), 'dry-report.txt', false);
  await userEvent.click(screen.getByLabelText('First row is header'));
  expect(within(table).getByRole('columnheader', { name: 'uri' })).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText('Filter report rows'), { target: { value: '/b' } });
  expect(within(table).queryByText('/a.xml')).toBeNull();
  const divider = screen.getByRole('separator', { name: 'Resize report and log' });
  expect(divider).toHaveAttribute('aria-valuenow', '75');
  fireEvent.keyDown(divider, { key: 'ArrowUp' });
  expect(divider).toHaveAttribute('aria-valuenow', '70');
  await userEvent.click(screen.getByRole('button', { name: 'Enlarge log' }));
  expect(screen.queryByRole('region', { name: 'Report pane' })).toBeNull();
  await userEvent.click(screen.getByRole('button', { name: 'Restore split view' }));
  expect(screen.getByRole('region', { name: 'Report pane' })).toBeInTheDocument();
  await userEvent.click(screen.getByRole('button', { name: 'Files' }));
  await userEvent.selectOptions(screen.getByLabelText('Run artifact'), 'job.options');
  expect(await screen.findByLabelText('Run output')).toHaveTextContent('THREAD-COUNT=4');
});

it('preserves multiline exceptions and quoted report fields', () => {
  const records = corbiLogRecords(jul);
  expect(records[0]).toEqual({ time: '1:27:54 PM', level: 'INFO', message: 'runtime arguments = --add-modules=ALL-DEFAULT' });
  expect(records.find(record => record.level === 'SEVERE')?.message).toContain('\n  at some.stack.frame');
  expect(reportRows('uri,details\r\n/a.xml,"has, commas\nand ""quotes"""', ',')).toEqual([['uri', 'details'], ['/a.xml', 'has, commas\nand "quotes"']]);
});
