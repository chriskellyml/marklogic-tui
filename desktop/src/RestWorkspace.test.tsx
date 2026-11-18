import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import { rewriterApi } from './rewriter';
import { openapiApi } from './openapi';
import { RestWorkspace } from './RestWorkspace';
import { restApi, restStorageKey, type RestResponse } from './rest-api';

vi.mock('./rewriter', async original => ({ ...await original<typeof import('./rewriter')>(), rewriterApi: { load: vi.fn() } }));
vi.mock('./openapi', async original => ({ ...await original<typeof import('./openapi')>(), openapiApi: { discover: vi.fn().mockResolvedValue([]), load: vi.fn() } }));
vi.mock('./rest-api', async original => ({ ...await original<typeof import('./rest-api')>(), restApi: { execute: vi.fn() } }));
vi.mock('./LogsWorkspace', () => ({ LogsWorkspace: ({ active, preferredPort }: { active: boolean; preferredPort: string }) => <div data-testid="embedded-logs" data-active={String(active)} data-port={preferredPort}>Log viewer</div> }));
vi.mock('./QueryEditor', () => ({ QueryEditor: ({ source, onChange, disabled, label }: { source: string; onChange: (value: string) => void; disabled: boolean; label: string }) => <textarea aria-label={label} value={source} disabled={disabled} onChange={event => onChange(event.target.value)} /> }));
vi.mock('./RestResponseViewer', () => ({ RestResponseViewer: ({ source }: { source: string }) => <pre aria-label="Response body">{source}</pre> }));

const connection = { server: 'local', appServer: 'api', database: 'Content' };
const servers = [{ name: 'local', endpoint: 'http://localhost:8000', username: 'admin', authType: 'digest' as const, appServers: [{ name: 'api', port: 8010, secure: false }] }, { name: 'staging', endpoint: 'https://stage.example:8000', username: 'stage-user', authType: 'basic' as const, appServers: [] }];
const response: RestResponse = { url: 'http://localhost:8010/v1/search?format=json', status: 400, statusText: 'Bad Request', headers: [['content-type', 'application/json']], body: '{"errorResponse":{"message":"Invalid query"}}', elapsedMs: 37, size: 51, truncated: false };
const onDirty = vi.fn();
const props = { active: true, connection, servers, onDirty };

beforeEach(() => { vi.mocked(openapiApi.discover).mockResolvedValue([]); localStorage.clear(); vi.clearAllMocks(); vi.mocked(restApi.execute).mockResolvedValue(response); });

it('uses path parameters and environment variables across saved requests', async () => {
  const view = render(<RestWorkspace {...props} />);
  fireEvent.change(screen.getByLabelText('Request path'), { target: { value: '/%%version%%/items/{id}' } });
  fireEvent.change(screen.getByLabelText('path parameter 1 value'), { target: { value: '%%item%%' } });
  await userEvent.click(screen.getByRole('button', { name: 'Add global variable' }));
  fireEvent.change(screen.getByLabelText('Global variable 1 name'), { target: { value: 'version' } });
  fireEvent.change(screen.getByLabelText('Global variable 1 value'), { target: { value: 'v1' } });
  await userEvent.click(screen.getByRole('button', { name: 'Add environment variable' }));
  fireEvent.change(screen.getByLabelText('Environment variable 1 name'), { target: { value: 'item' } });
  fireEvent.change(screen.getByLabelText('Environment variable 1 value'), { target: { value: 'a/b' } });
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenCalledWith(connection, expect.objectContaining({ path: expect.stringContaining('/v1/items/a%2Fb') })));
  view.unmount();
  render(<RestWorkspace {...props} />);
  expect(screen.getByLabelText('Environment variable 1 value')).toHaveValue('a/b');
  expect(screen.getByLabelText('path parameter 1 value')).toHaveValue('%%item%%');
});

it('sends with the selected connection and shows HTTP error responses with headers and origin', async () => {
  render(<RestWorkspace {...props} />);
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenCalledWith(connection, { method: 'GET', path: '/v1/search?q=&format=json&pageLength=10', headers: [['Accept', 'application/json']], body: null }));
  expect(await screen.findByText('400 Bad Request')).toBeInTheDocument();
  expect(screen.getByLabelText('Response body')).toHaveTextContent('Invalid query');
  expect(screen.getByText('local · api · admin')).toBeInTheDocument();
  await userEvent.click(within(screen.getByRole('navigation', { name: 'Response sections' })).getByRole('button', { name: /Headers/ }));
  expect(screen.getByText('content-type')).toBeInTheDocument();
});

it('changes the base URL with the environment, retaining the draft and prior response origin without sending', async () => {
  const view = render(<RestWorkspace {...props} />);
  fireEvent.change(screen.getByLabelText('Request path'), { target: { value: '/v1/documents?uri=example.json' } });
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await screen.findByText('400 Bad Request');
  const staging = { server: 'staging', appServer: null, database: 'Documents' };
  view.rerender(<RestWorkspace {...props} connection={staging} />);
  expect(screen.getByLabelText('REST base URL')).toHaveTextContent('https://stage.example:8000');
  expect(screen.getByLabelText('Request path')).toHaveValue('/v1/documents?uri=example.json');
  expect(screen.getByText('local · api · admin')).toBeInTheDocument();
  expect(restApi.execute).toHaveBeenCalledTimes(1);
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenLastCalledWith(staging, expect.anything()));
});

it('saves edits across remounts, duplicates and deletes requests, and sends a starter body', async () => {
  const view = render(<RestWorkspace {...props} />);
  await userEvent.click(screen.getByRole('button', { name: 'Write a document' }));
  fireEvent.change(screen.getByLabelText('Request name'), { target: { value: 'Save sample' } });
  await waitFor(() => expect(localStorage.getItem(restStorageKey)).toContain('Save sample'));
  view.unmount();
  render(<RestWorkspace {...props} />);
  await userEvent.click(screen.getByRole('button', { name: /PUT Save sample/ }));
  await userEvent.click(screen.getByRole('button', { name: 'Duplicate request' }));
  expect(screen.getByLabelText('Request name')).toHaveValue('Save sample copy');
  await userEvent.click(screen.getByRole('button', { name: 'Delete request' }));
  expect(screen.queryByRole('button', { name: /Save sample copy/ })).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole('button', { name: /PUT Save sample/ }));
  fireEvent.keyDown(screen.getByLabelText('Request path'), { key: 'Enter', ctrlKey: true });
  await waitFor(() => expect(restApi.execute).toHaveBeenCalledWith(connection, expect.objectContaining({ method: 'PUT', path: '/v1/documents?uri=%2Fexample.json', body: '{\n  "message": "Hello, MarkLogic"\n}' })));
});

it('prevents concurrent sends and reports transport errors', async () => {
  let reject!: (error: Error) => void;
  vi.mocked(restApi.execute).mockReturnValue(new Promise((_resolve, fail) => { reject = fail; }));
  render(<RestWorkspace {...props} />);
  const path = screen.getByLabelText('Request path');
  fireEvent.keyDown(path, { key: 'Enter', ctrlKey: true });
  fireEvent.keyDown(path, { key: 'Enter', ctrlKey: true });
  expect(restApi.execute).toHaveBeenCalledTimes(1);
  reject(new Error('Connection refused'));
  expect(await screen.findByRole('alert')).toHaveTextContent('Connection refused');
  expect(screen.getByRole('button', { name: 'Send' })).toBeEnabled();
});

it('saves and restores complete named sets and keeps them when duplicating a request', async () => {
  const view = render(<RestWorkspace {...props} />);
  fireEvent.change(screen.getByLabelText('parameter 1 value'), { target: { value: 'first query' } });
  await userEvent.click(within(screen.getByRole('navigation', { name: 'Request sections' })).getByRole('button', { name: /Headers/ }));
  fireEvent.change(screen.getByLabelText('header 1 value'), { target: { value: 'application/xml' } });
  await userEvent.click(within(screen.getByRole('navigation', { name: 'Request sections' })).getByRole('button', { name: /Body/ }));
  await userEvent.click(screen.getByLabelText(/Send request body/));
  fireEvent.change(screen.getByLabelText('Request body'), { target: { value: 'saved body' } });
  await userEvent.type(screen.getByLabelText('Set name'), 'Example');
  await userEvent.click(within(screen.getByLabelText('Saved request sets')).getByRole('button', { name: 'Save' }));
  fireEvent.change(screen.getByLabelText('Request body'), { target: { value: 'changed' } });
  await userEvent.click(screen.getByRole('button', { name: 'Duplicate request' }));
  view.unmount();
  render(<RestWorkspace {...props} />);
  await userEvent.click(screen.getByRole('button', { name: /GET Search documents copy/ }));
  await userEvent.click(screen.getByRole('button', { name: 'Example' }));
  await userEvent.click(within(screen.getByRole('navigation', { name: 'Request sections' })).getByRole('button', { name: /Body/ }));
  expect(screen.getByLabelText('Request body')).toHaveValue('saved body');
  expect(screen.getByLabelText(/Send request body/)).toBeChecked();
  await userEvent.click(within(screen.getByRole('navigation', { name: 'Request sections' })).getByRole('button', { name: /Headers/ }));
  expect(screen.getByLabelText('header 1 value')).toHaveValue('application/xml');
  await userEvent.click(within(screen.getByRole('navigation', { name: 'Request sections' })).getByRole('button', { name: /Params/ }));
  expect(screen.getByLabelText('parameter 1 value')).toHaveValue('first query');
  await userEvent.click(screen.getByRole('button', { name: 'Delete set Example' }));
  expect(screen.queryByRole('button', { name: 'Example' })).not.toBeInTheDocument();
});

it('organizes requests in persistent collections with Scratch as the default', async () => {
  const view = render(<RestWorkspace {...props} />);
  expect(screen.getByLabelText('Request collection')).toHaveValue('Scratch');
  await userEvent.type(screen.getByLabelText('New collection name'), 'Search API');
  await userEvent.click(screen.getByRole('button', { name: 'Create collection' }));
  expect(screen.getByLabelText('Request collection')).toHaveValue('Search API');
  await userEvent.click(screen.getByRole('button', { name: 'Duplicate request' }));
  view.unmount();
  render(<RestWorkspace {...props} />);
  expect(screen.getByLabelText('Request collection')).toHaveValue('Search API');
  expect(screen.getByText('Search API', { selector: 'strong' })).toBeInTheDocument();
  await userEvent.selectOptions(screen.getByLabelText('Request collection'), 'Scratch');
  expect(screen.getByLabelText('Request collection')).toHaveValue('Scratch');
});


it('switches between response, logs and split while following the selected environment', async () => {
  const view = render(<RestWorkspace {...props} />);
  expect(screen.queryByTestId('embedded-logs')).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole('button', { name: 'Show REST logs' }));
  expect(screen.getByTestId('embedded-logs')).toHaveAttribute('data-port', '8010');
  expect(screen.getByTestId('embedded-logs')).toHaveAttribute('data-active', 'true');
  expect(screen.queryByText('Ready to make a request')).not.toBeVisible();
  await userEvent.click(screen.getByRole('button', { name: 'Split response and logs' }));
  expect(screen.getByText('Ready to make a request')).toBeVisible();
  view.rerender(<RestWorkspace {...props} connection={{ server: 'staging', appServer: null, database: 'Documents' }} />);
  expect(screen.getByTestId('embedded-logs')).toHaveAttribute('data-port', '8000');
  await userEvent.click(screen.getByRole('button', { name: 'Show REST response' }));
  expect(screen.getByTestId('embedded-logs')).toHaveAttribute('data-active', 'true');
  expect(screen.getByText('Ready to make a request')).toBeVisible();
});

it('nests collections, moves requests and branches, and persists folding and pinned collections', async () => {
  const view = render(<RestWorkspace {...props} />);
  await userEvent.type(screen.getByLabelText('New collection name'), 'Projects/API');
  await userEvent.click(screen.getByRole('button', { name: 'Create collection' }));
  expect(screen.getByRole('button', { name: 'Collection Projects/API' })).toBeVisible();
  await userEvent.click(screen.getByRole('button', { name: 'Collection Projects' }));
  expect(screen.queryByRole('button', { name: 'Collection Projects/API' })).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole('button', { name: 'Collection Projects' }));
  await userEvent.click(screen.getByRole('button', { name: 'Pin collection Projects/API' }));
  expect(within(screen.getByLabelText('Pinned collections')).getByRole('button', { name: 'Collection Projects/API' })).toBeVisible();
  const transfer = { effectAllowed: '', dropEffect: '', setData: vi.fn() };
  const request = screen.getAllByRole('button', { name: /GET Search documents/ })[0];
  fireEvent.dragStart(request, { dataTransfer: transfer });
  const scratch = screen.getByRole('button', { name: 'Collection Scratch' }).parentElement!;
  fireEvent.dragOver(scratch, { dataTransfer: transfer });
  fireEvent.drop(scratch, { dataTransfer: transfer });
  expect(screen.getByLabelText('Request collection')).toHaveValue('Scratch');
  await userEvent.selectOptions(screen.getByLabelText('Request collection'), 'Projects/API');
  const projects = screen.getByRole('button', { name: 'Collection Projects' }).parentElement!;
  fireEvent.dragStart(projects, { dataTransfer: transfer });
  fireEvent.dragOver(scratch, { dataTransfer: transfer });
  fireEvent.drop(scratch, { dataTransfer: transfer });
  expect(screen.getAllByRole('button', { name: 'Collection Scratch/Projects/API' })).toHaveLength(2);
  expect(screen.getByLabelText('Request collection')).toHaveValue('Scratch/Projects/API');
  expect(screen.getByRole('button', { name: 'Unpin collection Scratch/Projects/API' })).toBeInTheDocument();
  view.unmount();
  render(<RestWorkspace {...props} />);
  expect(screen.getByRole('button', { name: 'Unpin collection Scratch/Projects/API' })).toBeInTheDocument();
  expect(screen.getByLabelText('Request collection')).toHaveValue('Scratch/Projects/API');
  await userEvent.click(screen.getByRole('button', { name: 'Unpin collection Scratch/Projects/API' }));
  expect(screen.queryByLabelText('Pinned collections')).not.toBeInTheDocument();
});


it('discovers multiple module specs and populates the request editor from their operations', async () => {
  vi.mocked(openapiApi.discover).mockResolvedValue(['/api/openapi.yml', '/other/openapi.yaml']);
  vi.mocked(openapiApi.load).mockImplementation(async (_connection, _database, uri) => ({ openapi: '3.0.0', info: { title: uri }, servers: [{ url: 'https://ignore.example/service' }], paths: { '/pets/{id}': { get: { summary: uri.includes('other') ? 'Other pet' : 'Read pet', tags: ['Pets'], parameters: [{ in: 'path', name: 'id', required: true, example: '42' }] } } } }));
  render(<RestWorkspace {...props} modulesDatabase="ApplicationModules" />);
  await userEvent.click(screen.getByRole('button', { name: 'OpenAPI' }));
  await screen.findByRole('button', { name: /GET Read pet/ });
  expect(openapiApi.discover).toHaveBeenCalledWith(connection, 'ApplicationModules');
  await userEvent.click(screen.getByRole('button', { name: /GET Read pet/ }));
  expect(screen.getByLabelText('Request path')).toHaveValue('/service/pets/{id}');
  expect(screen.getByLabelText('path parameter 1 value')).toHaveValue('42');
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenCalledWith(connection, expect.objectContaining({ path: '/service/pets/42' })));
  await userEvent.selectOptions(screen.getByLabelText('OpenAPI specification'), '/other/openapi.yaml');
  await userEvent.click(await screen.findByRole('button', { name: /GET Other pet/ }));
  expect(screen.getByLabelText('Request name')).toHaveValue('Other pet');
});


it('loads the configured rewriter on entry, starts public requests, refreshes and follows environments', async () => {
  vi.mocked(rewriterApi.load).mockResolvedValue({ path: '/rewriter.xml', root: '/', database: 'ApplicationModules', message: '', xml: '<rewriter xmlns="http://marklogic.com/xdmp/rewriter"><match-path matches="^/(v1|LATEST)/api/document$"><add-query-param name="internal">document</add-query-param><match-method any-of="GET PUT"><dispatch>/internal.xqy</dispatch></match-method></match-path></rewriter>' });
  const view = render(<RestWorkspace {...props} />);
  expect(rewriterApi.load).not.toHaveBeenCalled();
  await userEvent.click(screen.getByRole('button', { name: 'Rewriter' }));
  await userEvent.click(await screen.findByRole('button', { name: /GET.*api\/document/ }));
  expect(rewriterApi.load).toHaveBeenCalledWith(connection);
  expect(screen.getByLabelText('Request path')).toHaveValue('/{version}/api/document');
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenCalledWith(connection, expect.objectContaining({ path: '/v1/api/document', headers: expect.any(Array) })));
  vi.mocked(rewriterApi.load).mockResolvedValue({ path: '', root: '/', database: 'Modules', xml: '', message: 'This app server has no URL rewriter configured.' });
  await userEvent.click(screen.getByRole('button', { name: 'Refresh rewriter' }));
  expect(await screen.findByText('This app server has no URL rewriter configured.')).toBeVisible();
  view.rerender(<RestWorkspace {...props} connection={{ server: 'staging', appServer: null, database: 'Documents' }} />);
  await waitFor(() => expect(rewriterApi.load).toHaveBeenLastCalledWith({ server: 'staging', appServer: null, database: 'Documents' }));
});


it('uses temporary override credentials for REST only and clears them when switching environments', async () => {
  const view = render(<RestWorkspace {...props} />);
  await userEvent.click(screen.getByRole('button', { name: 'REST user: admin' }));
  const dialog = screen.getByRole('dialog', { name: 'REST user' });
  await userEvent.clear(within(dialog).getByLabelText('REST override username'));
  await userEvent.type(within(dialog).getByLabelText('REST override username'), 'analyst');
  await userEvent.type(within(dialog).getByLabelText('REST override password'), 'temporary-secret');
  await userEvent.click(within(dialog).getByRole('button', { name: 'Use this user' }));
  expect(screen.getByRole('button', { name: 'REST user override: analyst' })).toHaveClass('rest-user-override');
  expect(Array.from({ length: localStorage.length }, (_, i) => localStorage.getItem(localStorage.key(i)!)).join(' ')).toContain('temporary-secret');
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenLastCalledWith(connection, expect.anything(), { username: 'analyst', password: 'temporary-secret', authType: undefined }));
  expect(await screen.findByText('local · api · analyst')).toBeInTheDocument();
  const staging = { server: 'staging', appServer: null, database: 'Documents' };
  view.rerender(<RestWorkspace {...props} connection={staging} />);
  expect(screen.getByRole('button', { name: 'REST user: stage-user' })).not.toHaveClass('rest-user-override');
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenLastCalledWith(staging, expect.anything()));
  view.rerender(<RestWorkspace {...props} />);
  expect(screen.getByRole('button', { name: 'REST user: admin' })).toBeInTheDocument();
});

it('can cancel, remove an override, and choose a scheme for application-level profiles', async () => {
  const appServers = [{ ...servers[0], authType: 'application-level' as const }];
  render(<RestWorkspace {...props} servers={appServers} />);
  await userEvent.click(screen.getByRole('button', { name: 'REST user: admin' }));
  expect(screen.getByLabelText('REST override authentication')).toHaveValue('digestbasic');
  await userEvent.selectOptions(screen.getByLabelText('REST override authentication'), 'basic');
  await userEvent.type(screen.getByLabelText('REST override password'), 'temporary-secret');
  await userEvent.click(screen.getByRole('button', { name: 'Cancel' }));
  expect(screen.getByRole('button', { name: 'REST user: admin' })).toBeInTheDocument();
  await userEvent.click(screen.getByRole('button', { name: 'REST user: admin' }));
  expect(screen.getByLabelText('REST override password')).toHaveValue('');
  await userEvent.selectOptions(screen.getByLabelText('REST override authentication'), 'basic');
  await userEvent.click(screen.getByRole('button', { name: 'Use this user' }));
  await userEvent.click(screen.getByRole('button', { name: 'Send' }));
  await waitFor(() => expect(restApi.execute).toHaveBeenCalledWith(connection, expect.anything(), { username: 'admin', password: '', authType: 'basic' }));
  await userEvent.click(screen.getByRole('button', { name: 'REST user override: admin' }));
  await userEvent.click(screen.getByRole('button', { name: 'Use environment user' }));
  expect(screen.getByRole('button', { name: 'REST user: admin' })).toBeInTheDocument();
});
