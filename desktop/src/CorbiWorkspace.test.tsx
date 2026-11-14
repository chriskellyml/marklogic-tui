import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import { CorbiWorkspace } from './CorbiWorkspace';
import { corbiApi, type CorbiRun } from './corbi-api';

vi.mock('./corbi-api', async original => ({
  ...await original<typeof import('./corbi-api')>(),
  corbiApi: { rename: vi.fn(), createJob: vi.fn(), open: vi.fn(), source: vi.fn(), save: vi.fn(), status: vi.fn(), history: vi.fn(),
    artifacts: vi.fn(), output: vi.fn(), start: vi.fn(), confirmWet: vi.fn() },
}));
vi.mock('./QueryEditor', () => ({
  QueryEditor: ({ source, onChange }: { source: string; onChange: (value: string) => void }) =>
    <textarea aria-label="CoRBi source" value={source} onChange={e => onChange(e.target.value)}/>,
}));
const props = { active: true, onDirty: vi.fn(), connection: { server: 'local', appServer: 'rest', database: 'Content' }, modulesDatabase: 'Modules' };
const entry: CorbiRun = { id: '20261114120000', historyEnv: 'local', connection: props.connection, modulesDatabase: 'Modules',
  project: 'repair', job: 'index.job', wet: false, status: 'completed' };
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(corbiApi.open).mockResolvedValue({ root: '/data', permissions: { repair: { 'index.job': { production: false } } },
    projects: [{ name: 'repair', jobs: ['index.job'], scripts: [] }] });
  vi.mocked(corbiApi.source).mockResolvedValue('THREAD-COUNT=3');
  vi.mocked(corbiApi.status).mockResolvedValue(null);
  vi.mocked(corbiApi.history).mockResolvedValue([{ id: entry.id, environment: 'local' }]);
  vi.mocked(corbiApi.artifacts).mockResolvedValue(['dry-output.log', 'dry-report.txt']);
  vi.mocked(corbiApi.output).mockImplementation(async (_root, _project, _entry, file) => file === 'dry-report.txt' ? 'sample URI' : 'dry run finished');
});

it('binds runs and promotion to the shared Workbench target and displays the selected artifact', async () => {
  const page = render(<CorbiWorkspace {...props}/>);
  await screen.findByDisplayValue('THREAD-COUNT=3');
  vi.mocked(corbiApi.start).mockResolvedValue(entry);
  vi.mocked(corbiApi.status).mockResolvedValue(entry);
  await userEvent.click(screen.getByRole('button', { name: 'Dry run' }));
  expect(corbiApi.start).toHaveBeenCalledWith('/data', { project: 'repair', job: 'index.job', connection: props.connection, modulesDatabase: 'Modules', limit: 10, dryRunId: null });
  await screen.findByText('dry run finished');
  await userEvent.click(screen.getByRole('button', { name: 'Files' }));
  await screen.findByLabelText('Run artifact');
  await userEvent.selectOptions(screen.getByLabelText('Run artifact'), 'dry-report.txt');
  await screen.findByText('sample URI');

  page.rerender(<CorbiWorkspace {...props} connection={{ ...props.connection, database: 'Other' }}/>);
  expect(screen.queryByRole('button', { name: 'Execute wet run' })).toBeNull();
  page.rerender(<CorbiWorkspace {...props} connection={{ ...props.connection, server: 'production' }}/>);
  expect(screen.getByRole('button', { name: 'Dry run' })).toBeDisabled();
  page.rerender(<CorbiWorkspace {...props}/>);
  vi.mocked(corbiApi.confirmWet).mockResolvedValueOnce(false).mockResolvedValueOnce(true);
  await userEvent.click(screen.getByRole('button', { name: 'Execute wet run' }));
  expect(corbiApi.start).toHaveBeenCalledTimes(1);
  await userEvent.click(screen.getByRole('button', { name: 'Execute wet run' }));
  expect(corbiApi.start).toHaveBeenLastCalledWith('/data', expect.objectContaining({ connection: props.connection, dryRunId: entry.id, limit: null }));
});

it('retains edits when switching workspaces and surfaces conflicting saves', async () => {
  const page = render(<CorbiWorkspace {...props}/>);
  fireEvent.change(await screen.findByDisplayValue('THREAD-COUNT=3'), { target: { value: 'THREAD-COUNT=7' } });
  page.rerender(<CorbiWorkspace {...props} active={false}/>);
  page.rerender(<CorbiWorkspace {...props}/>);
  await waitFor(() => expect(props.onDirty).toHaveBeenLastCalledWith(true));
  vi.mocked(corbiApi.save).mockRejectedValueOnce('File changed on disk');
  await userEvent.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('File changed on disk');
  expect(screen.getByLabelText('CoRBi source')).toHaveValue('THREAD-COUNT=7');
  expect(screen.getByRole('button', { name: 'Dry run' })).toBeDisabled();
});

it('opens a ready workspace without choosing a folder and creates a job in place', async () => {
  localStorage.clear();
  const catalog = { root: '/workbench/corbi', permissions: {}, projects: [{
    name: 'My jobs', jobs: ['preview-documents.job'],
    scripts: ['scripts/preview-documents/collect.xqy', 'scripts/preview-documents/process.xqy'],
  }] };
  vi.mocked(corbiApi.open).mockResolvedValue(catalog);
  vi.mocked(corbiApi.source).mockResolvedValue('URIS-MODULE=scripts/preview-documents/collect.xqy|ADHOC\nPROCESS-MODULE=scripts/preview-documents/process.xqy|ADHOC');
  render(<CorbiWorkspace {...props}/>);
  await waitFor(() => expect(screen.getByRole('button', { name: 'Dry run' })).toBeEnabled());
  expect(corbiApi.open).toHaveBeenCalledWith(null);
  await userEvent.click(screen.getByRole('button', { name: 'Collect URIs' }));
  await waitFor(() => expect(corbiApi.source).toHaveBeenLastCalledWith('/workbench/corbi', 'My jobs', 'scripts/preview-documents/collect.xqy'));
  await waitFor(() => expect(screen.getByRole('button', { name: 'Dry run' })).toBeEnabled());
  await userEvent.click(screen.getByRole('button', { name: 'New job' }));
  await userEvent.type(screen.getByLabelText('New job name'), 'transform');
  vi.mocked(corbiApi.createJob).mockResolvedValue({ ...catalog, projects: [{
    ...catalog.projects[0], jobs: [...catalog.projects[0].jobs, 'transform.job'],
    scripts: [...catalog.projects[0].scripts, 'scripts/transform/collect.xqy', 'scripts/transform/process.xqy'],
  }] });
  await userEvent.click(screen.getByRole('button', { name: 'Create job' }));
  expect(corbiApi.createJob).toHaveBeenCalledWith('/workbench/corbi', 'My jobs', 'transform');
  await screen.findByRole('button', { name: 'transform.job' });
  expect(screen.getByRole('button', { name: 'Dry run' })).toBeEnabled();
});

it('shows collapsible script folders and refreshes the selected script after renaming', async () => {
  vi.mocked(corbiApi.open).mockResolvedValue({ root: '/data', permissions: {}, projects: [{
    name: 'repair', jobs: ['index.job'], scripts: ['scripts/collect/find.xqy', 'scripts/process/change.xqy'],
  }] });
  vi.mocked(corbiApi.source).mockResolvedValue('URIS-MODULE=scripts/collect/find.xqy|ADHOC');
  render(<CorbiWorkspace {...props}/>);
  const folder = await screen.findByRole('button', { name: 'collect folder' });
  await userEvent.click(folder);
  expect(screen.queryByRole('button', { name: 'find.xqy' })).toBeNull();
  await userEvent.click(folder);
  await userEvent.click(screen.getByRole('button', { name: 'find.xqy' }));
  await userEvent.click(screen.getByRole('button', { name: 'Rename scripts/collect' }));
  fireEvent.change(screen.getByLabelText('New name'), { target: { value: 'uris' } });
  vi.mocked(corbiApi.rename).mockResolvedValue({ root: '/data', permissions: {}, projects: [{
    name: 'repair', jobs: ['index.job'], scripts: ['scripts/uris/find.xqy', 'scripts/process/change.xqy'],
  }] });
  await userEvent.click(screen.getByRole('button', { name: 'Rename' }));
  expect(corbiApi.rename).toHaveBeenCalledWith('/data', 'repair', 'scripts/collect', 'uris');
  await waitFor(() => expect(corbiApi.source).toHaveBeenCalledWith('/data', 'repair', 'scripts/uris/find.xqy'));
});
