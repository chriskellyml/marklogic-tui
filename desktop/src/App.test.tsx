import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type Workspace } from './api';
import App from './App';
import { modulesApi } from './modules-api';
vi.mock('./modules-api',async(original)=>({...await original<typeof import('./modules-api')>(),modulesApi:{cache:vi.fn(),open:vi.fn(),search:vi.fn(),checkout:vi.fn(),save:vi.fn(),push:vi.fn(),run:vi.fn(),confirmPush:vi.fn(),confirmDrop:vi.fn(),drop:vi.fn()}}));

vi.mock('./api', async (original) => ({
  ...await original<typeof import('./api')>(),
  api: {
    buildInfo: vi.fn().mockResolvedValue({build:'test',executable:'/Applications/test.app'}), updateApp: vi.fn(), restartUpdatedApp: vi.fn(), loadWorkspace: vi.fn(), queryFolder: vi.fn(), openQuery: vi.fn(), saveQuery: vi.fn(),
    listDatabases: vi.fn(), executeQuery: vi.fn(), chooseFolder: vi.fn(), discardChanges: vi.fn(),
    protectClose: vi.fn().mockResolvedValue(() => {}),
  },
}));

// Exercise workspace behavior independently of CodeMirror's browser layout engine.
vi.mock('./QueryEditor', () => ({
  QueryEditor: ({ source, onChange, disabled }: { source: string; onChange: (value: string) => void; disabled: boolean }) =>
    <textarea aria-label="Query source" value={source} onChange={(e) => onChange(e.target.value)} disabled={disabled} />,
}));

const workspace: Workspace = {
  servers: [{ name: 'local', endpoint: 'http://localhost:8000', appServers: [] }],
  activeServer: 'local', activeAppServer: null, activeDatabase: 'Content', configPath: '/config.toml',
  queryRoot: '/queries', trackedFolders: [],
};

beforeEach(() => {
  vi.clearAllMocks();
  window.localStorage.clear();
  vi.mocked(modulesApi.cache).mockResolvedValue([]);
  vi.mocked(modulesApi.open).mockResolvedValue({content:'1',original:'1'});
  vi.mocked(modulesApi.save).mockResolvedValue(undefined);
  vi.mocked(modulesApi.push).mockResolvedValue(undefined);
  vi.mocked(modulesApi.confirmPush).mockResolvedValue(true);
  vi.mocked(modulesApi.confirmDrop).mockResolvedValue(true);
  vi.mocked(modulesApi.drop).mockResolvedValue({content:'server copy',original:'server copy'});
  vi.mocked(api.loadWorkspace).mockResolvedValue(workspace);
  vi.mocked(api.queryFolder).mockResolvedValue({ root: '/queries', files: [
    { path: 'one.xqy', language: 'xquery' }, { path: 'two.sjs', language: 'javascript' },
  ] });
  vi.mocked(api.openQuery).mockResolvedValue('1 + 1');
  vi.mocked(api.discardChanges).mockResolvedValue(false);
  vi.mocked(api.saveQuery).mockResolvedValue(undefined);
});

async function ready() {
  render(<App />);
  await waitFor(() => expect(screen.getByRole('button', { name: /Run query/ })).toBeEnabled());
}

describe('query workspace', () => {
  it('automatically starts the installed update', async () => {
    let finish!: () => void;
    vi.mocked(api.updateApp).mockReturnValue(new Promise((resolve) => { finish = resolve; }));
    vi.mocked(api.restartUpdatedApp).mockResolvedValue(undefined);
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Update app' }));
    expect(screen.getByRole('button', { name: 'Updating…' })).toBeDisabled();
    expect(screen.getByRole('progressbar', { name: 'Application update progress' })).toBeInTheDocument();
    act(() => vi.mocked(api.updateApp).mock.calls[0][0]('Compiling workbench'));
    expect(screen.getByText('Compiling workbench')).toBeInTheDocument();
    finish();
    await waitFor(() => expect(api.restartUpdatedApp).toHaveBeenCalledOnce());
    expect(api.discardChanges).not.toHaveBeenCalled();
  });

  it('preserves unsaved edits and restarts without rebuilding after the user saves', async () => {
    vi.mocked(api.updateApp).mockResolvedValue(undefined);
    vi.mocked(api.restartUpdatedApp).mockResolvedValue(undefined);
    await ready();
    fireEvent.change(screen.getByLabelText('Query source'), { target: { value: 'unsaved work' } });
    await userEvent.click(screen.getByRole('button', { name: 'Update app' }));
    expect(await screen.findByText(/Save your query/)).toBeInTheDocument();
    expect(api.restartUpdatedApp).not.toHaveBeenCalled();
    expect(screen.getByLabelText('Query source')).toHaveValue('unsaved work');
    vi.mocked(api.discardChanges).mockResolvedValueOnce(true);
    await userEvent.click(screen.getByRole('button', { name: 'Restart updated app' }));
    await waitFor(() => expect(api.restartUpdatedApp).toHaveBeenCalledOnce());
    expect(api.updateApp).toHaveBeenCalledOnce();
  });

  it('shows update build failures', async () => {
    vi.mocked(api.updateApp).mockRejectedValueOnce(new Error('Build failed'));
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'Update app' }));
    expect(await screen.findByText('Update failed: Build failed')).toBeInTheDocument();
  });

  it('runs current editor contents against the selected saved connection and formats results', async () => {
    vi.mocked(api.executeQuery).mockResolvedValue({ items: ['{"answer":42}'], elapsedMs: 12 });
    await ready();
    fireEvent.change(screen.getByLabelText('Query source'), { target: { value: '42' } });
    await userEvent.click(screen.getByRole('button', { name: /Run query/ }));
    expect(api.executeQuery).toHaveBeenCalledWith({ server: 'local', appServer: null, database: 'Content' }, '42', 'javascript', null);
    expect(await screen.findByText(/"answer": 42/)).toBeInTheDocument();
    expect(screen.getByText('12 ms')).toBeInTheDocument();
  });

  it('keeps file edits in tabs without a discard prompt when switching files', async () => {
    await ready();
    await userEvent.click(screen.getByRole('button', { name: /one.xqy/ }));
    fireEvent.change(screen.getByLabelText('Query source'), { target: { value: 'unsaved work' } });
    await userEvent.click(screen.getByRole('button', { name: /two.sjs/ }));
    expect(api.discardChanges).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole('tab',{name:/one.xqy/}));
    expect(screen.getByLabelText('Query source')).toHaveValue('unsaved work');
    await userEvent.click(screen.getByRole('button',{name:'Close two.sjs'}));
    expect(screen.queryByRole('tab',{name:/two.sjs/})).not.toBeInTheDocument();
  });

  it('saves with the original disk contents and retains edits after a save conflict', async () => {
    vi.mocked(api.saveQuery).mockRejectedValueOnce('The file changed on disk');
    await ready();
    await userEvent.click(screen.getByRole('button', { name: /one.xqy/ }));
    fireEvent.change(screen.getByLabelText('Query source'), { target: { value: '2 + 2' } });
    await userEvent.click(screen.getByRole('button', { name: 'Save query' }));
    expect(api.saveQuery).toHaveBeenCalledWith('one.xqy', '2 + 2', '1 + 1');
    expect(await screen.findByRole('alert')).toHaveTextContent('The file changed on disk');
    expect(screen.getByLabelText('Query source')).toHaveValue('2 + 2');
    expect(screen.getByLabelText('Unsaved changes')).toBeInTheDocument();
  });

  it('renders server errors as text and permits another run', async () => {
    vi.mocked(api.executeQuery).mockRejectedValueOnce('<script>bad query</script>');
    await ready();
    await userEvent.click(screen.getByRole('button', { name: /Run query/ }));
    expect(await screen.findByRole('alert')).toHaveTextContent('<script>bad query</script>');
    expect(screen.getByRole('button', { name: /Run query/ })).toBeEnabled();
    expect(document.querySelector('script')).toBeNull();
  });

  it('prevents a second query while a request is pending', async () => {
    let finish!: (value: { items: string[]; elapsedMs: number }) => void;
    vi.mocked(api.executeQuery).mockReturnValue(new Promise((resolve) => { finish = resolve; }));
    await ready();
    await userEvent.click(screen.getByRole('button', { name: /Run query/ }));
    expect(screen.getByRole('button', { name: /Running/ })).toBeDisabled();
    fireEvent.keyDown(window, { key: 'Enter', ctrlKey: true });
    expect(api.executeQuery).toHaveBeenCalledTimes(1);
    finish({ items: [], elapsedMs: 4 });
    expect(await screen.findByText('Query completed with no output')).toBeInTheDocument();
  });
});


describe('fused query and module editor',()=>{
 async function moduleReady(){
  vi.mocked(modulesApi.cache).mockResolvedValue([{uri:'/module.xqy',dirty:false,unsynced:false}]);
  await ready();
  await userEvent.click(await screen.findByText('module.xqy'));
  await waitFor(()=>expect(screen.getByLabelText('Query source')).toHaveValue('1'));
 }
 it('uses one editor and syncs current edits without requiring Save or Compare',async()=>{
  await moduleReady();
  expect(screen.queryByRole('button',{name:'Modules'})).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'changed'}});
  expect(screen.getByText('UNSYNCED MODULE — sync or drop local changes')).toBeInTheDocument();
  const sync=screen.getByRole('button',{name:'Sync module to server'});
  expect(sync).toBeEnabled();
  await userEvent.click(sync);
  await waitFor(()=>expect(modulesApi.push).toHaveBeenCalledWith({connection:{server:'local',appServer:null,database:'Content'},database:'Modules',uri:'/module.xqy'},'changed'));
  expect(modulesApi.save).toHaveBeenCalledWith(expect.any(Object),'changed','1');
  expect(await screen.findByText('Module synced')).toBeInTheDocument();
 });
 it('preserves module edits locally when opening a file in the same editor',async()=>{
  await moduleReady();
  fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'keep me'}});
  await userEvent.click(screen.getByText('one.xqy'));
  await waitFor(()=>expect(screen.getByLabelText('Query source')).toHaveValue('1 + 1'));
  await waitFor(()=>expect(modulesApi.save).toHaveBeenCalledWith(expect.any(Object),'keep me','1'),{timeout:3500});
  expect(api.discardChanges).not.toHaveBeenCalled();
 });
 it('keeps the warning when sync fails and offers an explicit server restore',async()=>{
  await moduleReady();
  vi.mocked(modulesApi.push).mockRejectedValueOnce(new Error('remote changed'));
  fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'changed'}});
  await userEvent.click(screen.getByRole('button',{name:'Sync module to server'}));
  expect(await screen.findByText('remote changed')).toBeInTheDocument();
  expect(screen.getByText('UNSYNCED MODULE — sync or drop local changes')).toBeInTheDocument();
  await userEvent.click(screen.getByRole('button',{name:'Drop module changes'}));
  await waitFor(()=>expect(screen.getByLabelText('Query source')).toHaveValue('server copy'));
  expect(modulesApi.confirmDrop).toHaveBeenCalledOnce();
 });
});


it('autosaves only after idle and keeps modules out of execution and results',async()=>{
 vi.mocked(modulesApi.cache).mockResolvedValue([{uri:'/module.xqy',dirty:false,unsynced:false}]);
 await ready();
 await userEvent.click(await screen.findByText('module.xqy'));
 await waitFor(()=>expect(screen.getByLabelText('Query source')).toHaveValue('1'));
 expect(screen.queryByRole('button',{name:/Run query/})).not.toBeInTheDocument();
 expect(screen.queryByRole('region',{name:'Query results'})).not.toBeInTheDocument();
 vi.useFakeTimers();
 try {
   fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'first'}});
   await act(async()=>{await vi.advanceTimersByTimeAsync(1500);});
   expect(modulesApi.save).not.toHaveBeenCalled();
   fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'second'}});
   await act(async()=>{await vi.advanceTimersByTimeAsync(1999);});
   expect(modulesApi.save).not.toHaveBeenCalled();
   await act(async()=>{await vi.advanceTimersByTimeAsync(1);});
   expect(modulesApi.save).toHaveBeenCalledExactlyOnceWith(expect.any(Object),'second','1');
   expect(modulesApi.push).not.toHaveBeenCalled();
   expect(screen.getByText('UNSYNCED MODULE — sync or drop local changes')).toBeInTheDocument();
   await act(async()=>{await vi.advanceTimersByTimeAsync(5000);});
   expect(modulesApi.save).toHaveBeenCalledTimes(1);
 } finally {vi.useRealTimers();}
});

it('does not overwrite newer edits when an autosave completes',async()=>{
 await ready();
 await userEvent.click(screen.getByRole('button',{name:/one.xqy/}));
 let finish!:()=>void;
 vi.mocked(api.saveQuery).mockReturnValueOnce(new Promise<void>((resolve)=>{finish=resolve;}));
 vi.useFakeTimers();
 try {
   fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'first'}});
   await act(async()=>{await vi.advanceTimersByTimeAsync(2000);});
   fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'second'}});
   await act(async()=>{finish();await Promise.resolve();});
   expect(screen.getByLabelText('Query source')).toHaveValue('second');
   await act(async()=>{await vi.advanceTimersByTimeAsync(2000);});
   expect(api.saveQuery).toHaveBeenLastCalledWith('one.xqy','second','first');
 } finally {vi.useRealTimers();}
});


it('restores an autosaved scratch draft and keeps the last-tab editor usable',async()=>{
 const view=render(<App/>);
 await waitFor(()=>expect(screen.getByRole('button',{name:/Run query/})).toBeEnabled());
 vi.useFakeTimers();
 try {
   fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'recovered draft'}});
   await act(async()=>{await vi.advanceTimersByTimeAsync(2000);});
 } finally {vi.useRealTimers();}
 view.unmount();
 await ready();
 expect(screen.getByLabelText('Query source')).toHaveValue('recovered draft');
 await userEvent.click(screen.getByRole('button',{name:'Close scratch'}));
 fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'new draft'}});
 expect(screen.getByLabelText('Query source')).toHaveValue('new draft');
});

it('starts with a wider sidebar and supports keyboard resizing',async()=>{
 await ready();
 const divider=screen.getByRole('separator',{name:'Resize file sidebar'});
 expect(divider).toHaveAttribute('aria-valuenow','450');
 fireEvent.keyDown(divider,{key:'ArrowRight'});
 expect(divider).toHaveAttribute('aria-valuenow','470');
 fireEvent.keyDown(divider,{key:'Home'});
 expect(divider).toHaveAttribute('aria-valuenow','450');
});
