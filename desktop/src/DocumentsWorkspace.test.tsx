import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { DocumentsWorkspace } from './DocumentsWorkspace';
import { documentsApi, type DocumentDetail } from './documents-api';
import type { Connection } from './api';

vi.mock('./documents-api', async original => ({ ...await original<typeof import('./documents-api')>(), documentsApi: { browse: vi.fn(), collections: vi.fn(), inspect: vi.fn() } }));
vi.mock('./DocumentViewer', () => ({ DocumentViewer: ({ document }: { document: DocumentDetail }) => <div>{document.uri}<pre>{document.content}</pre><span>{JSON.stringify(document.metadata)}</span></div> }));
const connection: Connection = { server: 'local', appServer: null, database: 'Content' };
const detail = (uri: string): DocumentDetail => ({ uri, format: 'json', content: '{"answer":42}', collections: ['red'], metadata: { owner: 'team' }, properties: '', permissions: [], quality: 0, timestamp: '123', warnings: [] });

beforeEach(() => {
  vi.resetAllMocks(); localStorage.clear();
  vi.mocked(documentsApi.browse).mockResolvedValue({ entries: [{ uri: '/orders/', name: '/orders/', kind: 'folder' }], nextCursor: null });
  vi.mocked(documentsApi.collections).mockResolvedValue({ collections: ['red', 'blue'], next: null });
  vi.mocked(documentsApi.inspect).mockImplementation(async (_, uri) => detail(uri));
});

describe('Documents workspace', () => {
  it('defers requests until opened and drills into paged branches', async () => {
    const view = render(<DocumentsWorkspace active={false} connection={connection} />);
    expect(documentsApi.browse).not.toHaveBeenCalled();
    view.rerender(<DocumentsWorkspace active connection={connection} />);
    await userEvent.click(await screen.findByRole('button', { name: 'Expand /orders/' }));
    expect(documentsApi.browse).toHaveBeenLastCalledWith(connection, '/orders/', null, { mode: 'any', collections: [] });
    vi.mocked(documentsApi.browse).mockResolvedValueOnce({ entries: [{ uri: '/orders/a.json', name: 'a.json', kind: 'document' }], nextCursor: { start: '/orders/a.json', exclusive: true } });
    await userEvent.click(screen.getByRole('button', { name: 'Refresh documents' }));
    await userEvent.click(await screen.findByRole('button', { name: 'a.json' }));
    expect(await screen.findByText('{"answer":42}')).toBeInTheDocument();
    expect(screen.getByText('{"owner":"team"}')).toBeInTheDocument();
    vi.mocked(documentsApi.browse).mockResolvedValueOnce({ entries: [{ uri: '/orders/b.xml', name: 'b.xml', kind: 'document' }], nextCursor: null });
    await userEvent.click(screen.getByRole('button', { name: 'Next paths in root' }));
    expect(await screen.findByRole('button', { name: 'b.xml' })).toBeInTheDocument();
    expect(documentsApi.browse).toHaveBeenLastCalledWith(connection, '', { start: '/orders/a.json', exclusive: true }, { mode: 'any', collections: [] });
    await userEvent.click(screen.getByRole('button', { name: 'Previous paths in root' }));
    await waitFor(() => expect(documentsApi.browse).toHaveBeenLastCalledWith(connection, '', null, { mode: 'any', collections: [] }));
  });

  it('combines collections with AND, saves sets per database, and restores them', async () => {
    const view = render(<DocumentsWorkspace active connection={connection} />);
    await userEvent.click(screen.getByText('Choose collections'));
    await userEvent.click(await screen.findByRole('checkbox', { name: 'red' }));
    await userEvent.click(screen.getByRole('checkbox', { name: 'blue' }));
    await userEvent.click(screen.getByRole('button', { name: 'All (AND)' }));
    await waitFor(() => expect(documentsApi.browse).toHaveBeenLastCalledWith(connection, '', null, { mode: 'all', collections: ['red', 'blue'] }));
    fireEvent.change(screen.getByLabelText('Collection set name'), { target: { value: 'Both colors' } });
    await userEvent.click(screen.getByRole('button', { name: 'Save set' }));
    await userEvent.click(screen.getByRole('button', { name: 'Clear filters' }));
    await userEvent.selectOptions(screen.getByLabelText('Saved collection sets'), 'Both colors');
    expect(screen.getByRole('button', { name: 'All (AND)' })).toHaveAttribute('aria-pressed', 'true');
    view.unmount();
    render(<DocumentsWorkspace active connection={connection} />);
    expect(screen.getByRole('option', { name: 'Both colors' })).toBeInTheDocument();
  });

  it('ignores an old document response after switching databases', async () => {
    vi.mocked(documentsApi.browse).mockResolvedValue({ entries: [{ uri: '/a.json', name: 'a.json', kind: 'document' }], nextCursor: null });
    let finish!: (value: DocumentDetail) => void;
    vi.mocked(documentsApi.inspect).mockReturnValueOnce(new Promise(resolve => { finish = resolve; }));
    const view = render(<DocumentsWorkspace active connection={connection} />);
    await userEvent.click(await screen.findByRole('button', { name: 'a.json' }));
    view.rerender(<DocumentsWorkspace active connection={{ ...connection, database: 'Other' }} />);
    await act(async () => finish(detail('/old-response.json')));
    expect(screen.queryByText('/old-response.json')).not.toBeInTheDocument();
    expect(within(screen.getByLabelText('Document viewer')).getByText('Explore your database')).toBeInTheDocument();
  });

  it('shows failed requests and lets users retry paths or enter exact collections', async () => {
    vi.mocked(documentsApi.browse).mockRejectedValueOnce(new Error('URI lexicon disabled'));
    vi.mocked(documentsApi.collections).mockRejectedValueOnce(new Error('Collection lexicon disabled'));
    render(<DocumentsWorkspace active connection={connection} />);
    expect(await screen.findByText('URI lexicon disabled')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Retry paths' }));
    expect(await screen.findByRole('button', { name: 'Expand /orders/' })).toBeInTheDocument();
    await userEvent.click(screen.getByText('Choose collections'));
    expect(screen.getByText(/Collection lexicon disabled/)).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Collection prefix or URI'), { target: { value: 'exact/collection' } });
    await userEvent.click(screen.getByRole('button', { name: 'Add URI' }));
    await waitFor(() => expect(documentsApi.browse).toHaveBeenLastCalledWith(connection, '', null, { mode: 'any', collections: ['exact/collection'] }));
  });
});
