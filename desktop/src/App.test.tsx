import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type Workspace } from './api';
import App from './App';
import { modulesApi } from './modules-api';
import { mlcpApi } from './mlcp-api';
vi.mock('./modules-api',async(original)=>({...await original<typeof import('./modules-api')>(),modulesApi:{cache:vi.fn(),open:vi.fn(),search:vi.fn(),checkout:vi.fn(),save:vi.fn(),push:vi.fn(),run:vi.fn(),confirmPush:vi.fn(),confirmDrop:vi.fn(),drop:vi.fn()}}));
vi.mock('./mlcp-api', () => ({
  mlcpApi: { chooseFolder: vi.fn(), confirmPush: vi.fn(), transfer: vi.fn(), previewUris: vi.fn(), jobData: vi.fn(), archiveSize: vi.fn(), archiveFiles: vi.fn(), confirmDeleteJob: vi.fn(), confirmDeleteData: vi.fn(), deleteJobData: vi.fn() },
}));

vi.mock('./api', async (original) => ({
  ...await original<typeof import('./api')>(),
  api: {
    queryVariables: vi.fn().mockResolvedValue({variables:[],globals:{}}),
    buildInfo: vi.fn().mockResolvedValue({build:'test',executable:'/Applications/test.app'}), updateApp: vi.fn(), restartUpdatedApp: vi.fn(), loadWorkspace: vi.fn(), queryFolder: vi.fn(), openQuery: vi.fn(), saveQuery: vi.fn(), saveScratchQuery: vi.fn(),
    listDatabases: vi.fn().mockResolvedValue(null), executeQuery: vi.fn(), chooseFolder: vi.fn(), discardChanges: vi.fn(), renameQuery:vi.fn(),deleteQuery:vi.fn(),confirmDeleteQuery:vi.fn(),
    openCodeModels: vi.fn(), openCodeReviseQuery: vi.fn(),
    openCodeStartService: vi.fn(),
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
  vi.mocked(mlcpApi.jobData).mockResolvedValue({ archives: [], log: '' });
  vi.mocked(mlcpApi.archiveSize).mockResolvedValue(2048);
  vi.mocked(mlcpApi.archiveFiles).mockResolvedValue(7);
  vi.mocked(api.queryFolder).mockResolvedValue({ root: '/queries', files: [
    { path: 'one.xqy', language: 'xquery' }, { path: 'two.sjs', language: 'javascript' },
  ] });
  vi.mocked(api.openQuery).mockResolvedValue('1 + 1');
  vi.mocked(api.discardChanges).mockResolvedValue(false);
  vi.mocked(api.saveQuery).mockResolvedValue(undefined);
  vi.mocked(api.openCodeModels).mockResolvedValue([{ providerId: 'local', id: 'example', name: 'Example' }]);
  vi.mocked(api.openCodeStartService).mockResolvedValue(undefined);
});

async function ready() {
  render(<App />);
  await waitFor(() => expect(screen.getByRole('button', { name: /Run query/ })).toBeEnabled());
}

describe('query workspace', () => {
  it('keeps an OpenCode revision on its originating tab until applied', async () => {
    let complete!: (replacement: string) => void;
    vi.mocked(api.openCodeReviseQuery).mockReturnValueOnce(new Promise(resolve => { complete = resolve; }));
    await ready();
    await userEvent.click(screen.getByRole('button', { name: /^XQ one\.xqy$/ }));
    fireEvent.change(screen.getByLabelText('Query source'), { target: { value: '1 + 2' } });
    await userEvent.click(screen.getByRole('button', { name: 'OpenCode' }));
    await userEvent.click(await screen.findByRole('button', { name: 'Choose a model' }));
    await userEvent.click(screen.getByRole('button', { name: 'Example local / example' }));
    await userEvent.type(screen.getByRole('textbox', { name: 'OpenCode prompt' }), 'Add one to the result');
    await userEvent.click(screen.getByRole('button', { name: 'Generate revision' }));
    expect(api.openCodeReviseQuery).toHaveBeenCalledWith({ providerId: 'local', id: 'example' }, 'Add one to the result', '1 + 2', 'xquery');

    await userEvent.click(screen.getByRole('button', { name: /^JS two\.sjs$/ }));
    await act(async () => complete('1 + 2 + 1'));
    expect(screen.getByLabelText('Query source')).toHaveValue('1 + 1');
    expect(screen.queryByRole('button', { name: 'Apply to editor' })).toBeNull();
    await userEvent.click(screen.getByRole('tab', { name: /one\.xqy/ }));
    expect(screen.getByLabelText('Query source')).toHaveValue('1 + 2');
    expect(screen.getByLabelText('Proposed query')).toHaveTextContent('1 + 2 + 1');
    await userEvent.click(screen.getByRole('button', { name: 'Apply to editor' }));
    expect(screen.getByLabelText('Query source')).toHaveValue('1 + 2 + 1');
    expect(api.executeQuery).not.toHaveBeenCalled();
  });

  it('prevents an OpenCode revision from overwriting newer edits', async () => {
    let complete!: (replacement: string) => void;
    vi.mocked(api.openCodeReviseQuery).mockReturnValueOnce(new Promise(resolve => { complete = resolve; }));
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'OpenCode' }));
    await userEvent.click(await screen.findByRole('button', { name: 'Choose a model' }));
    await userEvent.click(screen.getByRole('button', { name: 'Example local / example' }));
    await userEvent.type(screen.getByRole('textbox', { name: 'OpenCode prompt' }), 'Revise this');
    await userEvent.click(screen.getByRole('button', { name: 'Generate revision' }));
    fireEvent.change(screen.getByLabelText('Query source'), { target: { value: 'new local edit' } });
    await act(async () => complete('old revision'));

    expect(screen.getByRole('button', { name: 'Apply to editor' })).toBeDisabled();
    expect(screen.getByLabelText('Query source')).toHaveValue('new local edit');
    await userEvent.click(screen.getByRole('button', { name: 'Discard revision' }));
    expect(screen.getByRole('button', { name: 'Generate revision' })).toBeEnabled();
  });

  it('searches models by provider and keeps favorites across tabs', async () => {
    vi.mocked(api.openCodeModels).mockResolvedValue([
      { providerId: 'first', id: 'alpha', name: 'Alpha' },
      { providerId: 'second', id: 'beta', name: 'Beta' },
    ]);
    await ready();
    await userEvent.click(screen.getByRole('button', { name: 'OpenCode' }));
    await userEvent.click(await screen.findByRole('button', { name: 'Choose a model' }));
    await userEvent.type(screen.getByRole('textbox', { name: 'Search OpenCode models' }), 'second');
    expect(screen.queryByRole('button', { name: 'Alpha first / alpha' })).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: 'Add Beta to favorites (second)' }));
    await userEvent.clear(screen.getByRole('textbox', { name: 'Search OpenCode models' }));
    const list = screen.getByLabelText('Available OpenCode models');
    expect(within(list).getAllByRole('button')[0]).toHaveTextContent('Beta');
    await userEvent.click(screen.getByRole('button', { name: /^XQ one\.xqy$/ }));
    await userEvent.click(screen.getByRole('button', { name: 'OpenCode' }));
    await userEvent.click(await screen.findByRole('button', { name: 'Choose a model' }));
    expect(screen.getByRole('button', { name: 'Remove Beta from favorites (second)' })).toHaveAttribute('aria-pressed', 'true');
  });

  it('switches recent server and app-server pairs and flips between the last two', async () => {
    vi.mocked(api.loadWorkspace).mockResolvedValue({ ...workspace, servers: [
      { name: 'local', endpoint: '', color: '#ffcc00', appServers: [{ name: 'api', port: 8000, secure: false, content_database: 'ApiContent', modules_database: 'ApiModules' }] },
      { name: 'staging', endpoint: '', color: '#ff0000', appServers: [{ name: 'web', port: 8010, secure: false, content_database: 'WebContent', modules_database: 'WebModules' }] },
    ] });
    await ready();
    expect((document.querySelector('.environment-border-edges') as HTMLElement).style.backgroundImage).toContain('#ffcc00');
    await userEvent.click(screen.getByTitle('Choose connection'));
    fireEvent.change(screen.getByLabelText('Database'), { target: { value: 'CustomContent' } });
    await userEvent.click(screen.getByRole('button', { name: 'Pin selection to 1' }));
    await userEvent.click(screen.getByRole('button', { name: 'OK' }));
    await userEvent.click(screen.getByTitle('Choose connection'));
    await userEvent.selectOptions(screen.getByLabelText('App server'), 'api');
    await userEvent.click(screen.getByRole('button', { name: 'Pin selection to 2' }));
    await userEvent.click(screen.getByRole('button', { name: 'OK' }));
    expect(screen.getByRole('button', { name: 'Pinned environment 1: local · Profile default' })).toHaveAttribute('title', '1 · local · Profile default');
    await userEvent.click(screen.getByTitle('Choose connection'));
    await userEvent.selectOptions(screen.getByLabelText('Server'), 'staging');
    await userEvent.selectOptions(screen.getByLabelText('App server'), 'web');
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('local · api');
    await userEvent.click(screen.getByRole('button', { name: 'OK' }));
    expect((document.querySelector('.environment-border-edges') as HTMLElement).style.backgroundImage).toContain('#ff0000');
    expect(screen.getByRole('button', { name: 'Pinned environment 2: local · api' })).toBeEnabled();
    expect(screen.queryByRole('button', { name: /staging · Profile default/ })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Pinned environment 2: local · api' }));
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('local · api');
    await userEvent.click(screen.getByTitle('Choose connection'));
    expect(screen.getByLabelText('Database')).toHaveValue('ApiContent');
    expect(screen.getByLabelText('Modules database')).toHaveValue('ApiModules');
    await userEvent.click(screen.getByRole('button', { name: 'Switch to previous environment: staging · web' }));
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('staging · web');
    await userEvent.click(screen.getByRole('button', { name: 'Switch to previous environment: local · api' }));
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('local · api');
    await userEvent.click(screen.getByRole('button', { name: 'Pinned environment 1: local · Profile default' }));
    await userEvent.click(screen.getByTitle('Choose connection'));
    expect(screen.getByLabelText('Database')).toHaveValue('CustomContent');
  });

  it('restores the last selected environment and colors recent buttons across restarts', async () => {
    vi.mocked(api.loadWorkspace).mockResolvedValue({ ...workspace, servers: [
      { name: 'local', endpoint: '', color: '#44ddaa', appServers: [] },
      { name: 'staging', endpoint: '', color: '#ff4455', appServers: [{ name: 'web', port: 8010, secure: false }] },
    ] });
    const app = render(<App />);
    await waitFor(() => expect(screen.getByRole('button', { name: /Run query/ })).toBeEnabled());
    await userEvent.click(screen.getByTitle('Choose connection'));
    await userEvent.selectOptions(screen.getByLabelText('Server'), 'staging');
    await userEvent.selectOptions(screen.getByLabelText('App server'), 'web');
    fireEvent.change(screen.getByLabelText('Database'), { target: { value: 'Custom' } });
    fireEvent.change(screen.getByLabelText('Modules database'), { target: { value: 'CustomModules' } });
    await userEvent.click(screen.getByRole('button', { name: 'OK' }));
    expect(screen.getByRole('button', { name: 'Recent environment 1: local · Profile default' })).toHaveStyle({ backgroundColor: '#44ddaa', color: '#000' });
    app.unmount();

    render(<App />);
    await waitFor(() => expect(screen.getByTitle('Choose connection')).toHaveTextContent('staging · web'));
    expect(screen.getByRole('button', { name: 'Recent environment 1: local · Profile default' })).toHaveStyle({ backgroundColor: '#44ddaa', color: '#000' });
    await userEvent.click(screen.getByTitle('Choose connection'));
    expect(screen.getByLabelText('Database')).toHaveValue('Custom');
    expect(screen.getByLabelText('Modules database')).toHaveValue('CustomModules');
    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    await userEvent.click(screen.getByRole('button', { name: 'Recent environment 1: local · Profile default' }));
    expect(screen.getByRole('button', { name: 'Recent environment 1: staging · web' })).toHaveStyle({ backgroundColor: '#ff4455', color: '#000' });
  });

  it('shows a yellow warning border for an existing server without a configured color', async () => {
    await ready();
    expect((document.querySelector('.environment-border') as HTMLElement).style.backgroundImage).toContain('#ffcc00');
  });

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
    await userEvent.click(screen.getByRole('button', { name: /^XQ one\.xqy$/ }));
    fireEvent.change(screen.getByLabelText('Query source'), { target: { value: 'unsaved work' } });
    await userEvent.click(screen.getByRole('button', { name: /^JS two\.sjs$/ }));
    expect(api.discardChanges).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole('tab',{name:/one.xqy/}));
    expect(screen.getByLabelText('Query source')).toHaveValue('unsaved work');
    await userEvent.click(screen.getByRole('button',{name:'Close two.sjs'}));
    expect(screen.queryByRole('tab',{name:/two.sjs/})).not.toBeInTheDocument();
  });

  it('saves with the original disk contents and retains edits after a save conflict', async () => {
    vi.mocked(api.saveQuery).mockRejectedValueOnce('The file changed on disk');
    await ready();
    await userEvent.click(screen.getByRole('button', { name: /^XQ one\.xqy$/ }));
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
 it('favorites a downloaded module and reopens it from Favorites',async()=>{
  vi.mocked(modulesApi.cache).mockResolvedValue([{uri:'/module.xqy',dirty:false,unsynced:false}]);
  await ready();
  await userEvent.click(screen.getByRole('button',{name:'Modules'}));
  const favorite=await screen.findByRole('button',{name:'Favorite module /module.xqy'});
  await userEvent.click(favorite);
  expect(favorite).toHaveAttribute('aria-pressed','true');
  await userEvent.click(screen.getByRole('button',{name:/Favorites/}));
  await userEvent.click(screen.getByRole('button',{name:'/module.xqy'}));
  await waitFor(()=>expect(screen.getByLabelText('Query source')).toHaveValue('1'));
  expect(screen.getByRole('tab',{name:/module.xqy/})).toBeInTheDocument();
  await userEvent.click(screen.getByRole('button',{name:'Remove favorite module /module.xqy'}));
  expect(screen.queryByRole('button',{name:'Remove favorite module /module.xqy'})).not.toBeInTheDocument();
 });
 it('uses one editor and syncs current edits without requiring Save or Compare',async()=>{
  await moduleReady();
  expect(screen.getByRole('button',{name:'Modules'})).toBeInTheDocument();
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
 await userEvent.click(screen.getByRole('button',{name:/^XQ one\.xqy$/}));
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

it('switches explorer views while keeping the query editor and results visible', async()=>{
 await ready();
 fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'working draft'}});
 await userEvent.click(screen.getByRole('button',{name:'Modules'}));
 expect(screen.getByRole('region',{name:'Modules of interest'})).toBeVisible();
 expect(screen.getByLabelText('Query source')).toHaveValue('working draft');
 expect(screen.getByRole('region',{name:'Query results'})).toBeVisible();
 await userEvent.click(screen.getByRole('button',{name:'Recent'}));
 expect(screen.getByRole('region',{name:'Recently viewed files'})).toBeVisible();
 expect(screen.getByLabelText('Query source')).toHaveValue('working draft');
});

it('displays XML and renders HTML results in a sandbox', async () => {
 vi.mocked(api.executeQuery).mockResolvedValue({items:['<root><child>value</child></root>', '<p>Hello</p>'],elapsedMs:1});
 await ready();
 await userEvent.click(screen.getByRole('button',{name:/Run query/}));
 await screen.findByText(/<root>/);
 await userEvent.selectOptions(screen.getByLabelText('Result format'),'xml');
 expect(screen.getByText(/<root>/).textContent).toBe('<root>\n  <child>value</child>\n</root>');
 await userEvent.selectOptions(screen.getByLabelText('Result format'),'html');
 expect(screen.getByTitle('HTML result 2')).toHaveAttribute('sandbox', '');
 expect(screen.getByTitle('HTML result 2').getAttribute('srcdoc')).toContain('<p>Hello</p>');
});

it('formats and expands results while preserving the query editor',async()=>{
 vi.mocked(api.executeQuery).mockResolvedValue({items:['one,"two, three"'],elapsedMs:1});
 await ready();
 await userEvent.click(screen.getByRole('button',{name:/Run query/}));
 await screen.findByText(/two, three/);
 await userEvent.selectOptions(screen.getByLabelText('Result format'),'csv');
 expect(screen.getByRole('cell',{name:'two, three'})).toBeInTheDocument();
 const splitter=screen.getByRole('separator',{name:'Resize results'});
 const before=Number(splitter.getAttribute('aria-valuenow'));
 fireEvent.keyDown(splitter,{key:'ArrowUp'});
 expect(splitter).toHaveAttribute('aria-valuenow',String(before+20));
 await userEvent.click(screen.getByRole('button',{name:'Expand results'}));
 expect(screen.getByRole('button',{name:'Restore results size'})).toBeInTheDocument();
 fireEvent.keyDown(window,{key:'Escape'});
 expect(screen.getByRole('button',{name:'Expand results'})).toBeInTheDocument();
});

it('renames an open query after saving its edits',async()=>{
 vi.mocked(api.renameQuery).mockResolvedValue({path:'new/renamed.xqy',language:'xquery'});
 await ready();
 await userEvent.click(screen.getByRole('button',{name:/^XQ one\.xqy$/}));
 fireEvent.change(screen.getByLabelText('Query source'),{target:{value:'edited'}});
 await userEvent.click(screen.getByRole('button',{name:'Rename one.xqy'}));
 const name=screen.getByLabelText('New file name');await userEvent.clear(name);await userEvent.type(name,'new/renamed.xqy');
 await userEvent.click(screen.getByRole('button',{name:'Rename'}));
 await waitFor(()=>expect(api.renameQuery).toHaveBeenCalledWith('one.xqy','new/renamed.xqy','edited'));
 expect(api.saveQuery).toHaveBeenCalledWith('one.xqy','edited','1 + 1');
 expect(screen.getByRole('tab',{name:'renamed.xqy'})).toBeInTheDocument();
});

it('does not delete a query when confirmation is cancelled',async()=>{
 vi.mocked(api.confirmDeleteQuery).mockResolvedValue(false);
 await ready();
 await userEvent.click(screen.getByRole('button',{name:'Delete one.xqy'}));
 expect(api.confirmDeleteQuery).toHaveBeenCalledWith('one.xqy');
 expect(api.deleteQuery).not.toHaveBeenCalled();
});


it('shows the requested workspace navigation and keeps documents available', async () => {
  render(<App />);
  const nav = screen.getByRole('navigation', { name: 'Workspace' });
  expect(within(nav).getAllByRole('button').map(button => button.textContent)).toEqual([
    'Connection', 'Documents', 'Query', 'REST', 'Topology', 'Meters', 'CoRB', 'MLCP', 'Flux', 'Logs',
  ]);
  await userEvent.click(within(nav).getByRole('button', { name: 'REST' }));
  expect(screen.getByRole('region', { name: 'REST workspace' })).toBeVisible();
  await waitFor(() => expect(screen.getByLabelText('REST base URL')).toHaveTextContent('http://localhost:8000'));
  await userEvent.click(within(nav).getByRole('button', { name: 'MLCP' }));
  expect(screen.getByRole('heading', { name: 'MLCP' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Pull archive' })).toBeVisible();
  expect(screen.getByRole('button', { name: 'Push archive' })).toBeVisible();
  await userEvent.click(within(nav).getByRole('button', { name: 'Query' }));
  await userEvent.click(within(nav).getByRole('button', { name: 'Documents' }));
  expect(within(nav).getByRole('button', { name: 'Documents' })).toHaveAttribute('aria-pressed', 'true');
  expect(within(nav).getAllByRole('button').every(button => button.querySelector('svg[aria-hidden="true"]'))).toBe(true);
  await waitFor(() => expect(document.querySelector('.environment-border-edges')).toBeInTheDocument());
});

it('pulls from the active database and reuses the archive for independent MLCP destinations', async () => {
  vi.mocked(api.loadWorkspace).mockResolvedValue({
    ...workspace,
    servers: [...workspace.servers, {
      name: 'test', endpoint: 'http://test.example:8000',
      appServers: [{ name: 'XDBC', port: 8011, secure: true, content_database: 'TestContent' }],
    }],
  });
  const archive = '/transfers/mlcp-test/archive';
  vi.mocked(mlcpApi.chooseFolder).mockResolvedValue('/transfers');
  vi.mocked(mlcpApi.transfer).mockResolvedValue({ archive, parts: 1, output: 'Done' });
  vi.mocked(mlcpApi.confirmPush).mockResolvedValue(true);
  await ready();
  await userEvent.click(screen.getByRole('button', { name: 'MLCP' }));
  expect(screen.getByLabelText('Source server')).toHaveValue('local');
  expect(screen.getByLabelText('Source database')).toHaveValue('Content');
  await userEvent.selectOptions(screen.getByLabelText('Documents to pull'), 'collections');
  await userEvent.type(screen.getByLabelText('Collections', { exact: false }), 'orders');
  vi.mocked(mlcpApi.previewUris).mockResolvedValue(['/orders/one.json', '/orders/two.json']);
  await userEvent.click(screen.getByRole('button', { name: 'Preview URIs' }));
  expect(mlcpApi.previewUris).toHaveBeenCalledWith(
    { server: 'local', appServer: null, database: 'Content' }, { kind: 'collections', value: 'orders' },
  );
  expect(await screen.findByText('/orders/one.json')).toBeVisible();
  expect(mlcpApi.transfer).not.toHaveBeenCalled();
  await userEvent.click(screen.getByRole('button', { name: 'Choose export folder' }));
  await userEvent.click(screen.getByRole('button', { name: 'Pull archive' }));
  expect(mlcpApi.transfer).toHaveBeenNthCalledWith(1, { server: 'local', appServer: null, database: 'Content' }, {
    direction: 'pull', folder: '/transfers', selection: { kind: 'collections', value: 'orders' }, jobId: expect.any(String), replace: false,
  }, expect.any(Function));
  expect(await screen.findByText('Pull complete')).toBeVisible();
  expect(screen.getByRole('button', { name: 'Edit' })).toBeEnabled();
  expect(screen.getByLabelText('Source database')).toBeDisabled();
  expect(screen.getByRole('button', { name: 'Pull archive' })).toBeEnabled();
  expect(screen.getByLabelText('MLCP archive folder')).toHaveValue(archive);
  await userEvent.click(screen.getByRole('button', { name: /^Rename job / }));
  await userEvent.clear(screen.getByLabelText('Job name'));
  await userEvent.type(screen.getByLabelText('Job name'), 'Orders');
  fireEvent.blur(screen.getByLabelText('Job name'));
  await userEvent.click(screen.getByRole('button', { name: /^Rename archive / }));
  await userEvent.clear(screen.getByLabelText('Archive name'));
  await userEvent.type(screen.getByLabelText('Archive name'), 'Orders export');
  fireEvent.blur(screen.getByLabelText('Archive name'));
  await userEvent.selectOptions(screen.getByLabelText('Archive folder behavior'), 'replace');
  let finish!: (result: { archive: string; parts: number; output: string }) => void;
  vi.mocked(mlcpApi.transfer).mockImplementationOnce((_connection, _request, onOutput) => {
    onOutput('Export in progress');
    return new Promise(resolve => { finish = resolve; });
  });
  await userEvent.click(screen.getByRole('button', { name: 'Pull archive' }));
  expect(screen.getByText('Export in progress')).toBeVisible();
  expect(vi.mocked(mlcpApi.transfer).mock.calls[1][1]).toMatchObject({ direction: 'pull', replace: true, jobId: expect.any(String) });
  await act(async () => finish({ archive, parts: 1, output: 'Done' }));
  expect(screen.getByRole('button', { name: /^Orders export/ })).toBeVisible();
  vi.mocked(mlcpApi.chooseFolder).mockResolvedValueOnce('/transfers/other/archive');
  await userEvent.click(screen.getByRole('button', { name: 'Choose archive folder' }));
  await userEvent.click(screen.getByRole('button', { name: /^Orders export/ }));
  expect(screen.getByLabelText('MLCP archive folder')).toHaveValue(archive);

  await userEvent.selectOptions(screen.getByLabelText('Destination server'), 'test');
  await userEvent.selectOptions(screen.getByLabelText('Destination app server'), 'XDBC');
  expect(screen.getByLabelText('Destination database')).toHaveValue('TestContent');
  const push = screen.getByRole('button', { name: 'Push archive' });
  vi.mocked(mlcpApi.confirmPush).mockResolvedValueOnce(false);
  await userEvent.click(push);
  expect(mlcpApi.transfer).toHaveBeenCalledTimes(2);
  await userEvent.click(push);
  expect(mlcpApi.confirmPush).toHaveBeenLastCalledWith(archive, 'test / XDBC / TestContent / https://test.example:8011');
  expect(mlcpApi.transfer).toHaveBeenNthCalledWith(3, { server: 'test', appServer: 'XDBC', database: 'TestContent' }, {
    direction: 'push', folder: archive, copyPermissions: true,
  }, expect.any(Function));
  expect(screen.getByLabelText('Source server')).toHaveValue('local');

  await userEvent.click(within(screen.getByRole('group', { name: 'Push' })).getByRole('button', { name: 'Use active connection' }));
  await userEvent.click(push);
  expect(mlcpApi.transfer).toHaveBeenNthCalledWith(4, { server: 'local', appServer: null, database: 'Content' }, {
    direction: 'push', folder: archive, copyPermissions: true,
  }, expect.any(Function));
  vi.mocked(mlcpApi.jobData).mockResolvedValue({ archives: [{ path: archive, size: 2048, files: 7 }], log: 'Previous pull log' });
  await userEvent.click(screen.getByRole('button', { name: 'Rename job Orders' }));
  await userEvent.type(screen.getByLabelText('Job name'), '!');
  fireEvent.blur(screen.getByLabelText('Job name'));
  await waitFor(() => expect(screen.getByText('1 downloads')).toBeVisible());
  await userEvent.click(screen.getByRole('button', { name: /^Orders!/ }));
  expect(screen.getByText('Previous pull log')).toBeVisible();
  expect(within(screen.getByRole('button', { name: /^Orders export/ })).getByText(/2.0 KiB/)).toBeVisible();
  vi.mocked(mlcpApi.confirmDeleteJob).mockResolvedValueOnce(false).mockResolvedValueOnce(true);
  vi.mocked(mlcpApi.confirmDeleteData).mockResolvedValueOnce(false);
  await userEvent.click(screen.getByRole('button', { name: 'Delete job Orders!' }));
  expect(mlcpApi.confirmDeleteData).not.toHaveBeenCalled();
  await userEvent.click(screen.getByRole('button', { name: 'Delete job Orders!' }));
  await waitFor(() => expect(screen.queryByRole('button', { name: 'Delete job Orders!' })).toBeNull());
  expect(mlcpApi.deleteJobData).not.toHaveBeenCalled();
  expect(screen.getByRole('button', { name: /^Orders export/ })).toBeVisible();
});

it('runs an XQuery with sidebar values and restores them when reopening the tab', async () => {
 const source = 'declare variable $limit as xs:int external; $limit';
 vi.mocked(api.openQuery).mockResolvedValue(source);
 vi.mocked(api.queryVariables).mockResolvedValue({variables:[{name:'limit',sequenceType:'xs:int',defaultValue:null}],globals:{LIMIT:'25'}});
 vi.mocked(api.executeQuery).mockResolvedValue({items:['25'],elapsedMs:1});
 await ready();
 await userEvent.click(screen.getByTitle('one.xqy'));
 await userEvent.type(await screen.findByLabelText('Value for $limit'),'%%LIMIT%%');
 await userEvent.click(screen.getByRole('button',{name:/Run query/}));
 await waitFor(()=>expect(api.executeQuery).toHaveBeenCalledWith(expect.anything(),source,'xquery','one.xqy',{limit:{value:'%%LIMIT%%'}}));
 await userEvent.click(screen.getByTitle('two.sjs'));
 expect(screen.queryByLabelText('External variables')).not.toBeInTheDocument();
 await userEvent.click(screen.getByRole('tab',{name:'one.xqy'}));
 expect(await screen.findByLabelText('Value for $limit')).toHaveValue('%%LIMIT%%');
});

it('focuses files and folders, opens focused files, and clears focus', async () => {
 vi.mocked(api.queryFolder).mockResolvedValue({root:'/queries',folders:['api'],files:[{path:'api/get.xqy',language:'xquery'},{path:'one.xqy',language:'xquery'}]});
 await ready();
 await userEvent.click(screen.getByRole('button',{name:'Focus folder api'}));
 await userEvent.click(screen.getByRole('button',{name:'Focus file one.xqy'}));
 await userEvent.click(screen.getByRole('button',{name:'Favorites'}));
 const focused=screen.getByRole('region',{name:'Focused queries'});
 expect(within(focused).getByRole('button',{name:'Unfocus folder api'})).toBeInTheDocument();
 expect(within(focused).getByRole('button',{name:'Unfocus file one.xqy'})).toBeInTheDocument();
 await userEvent.click(within(focused).getByRole('button',{name:'XQ one.xqy'}));
 await waitFor(()=>expect(api.openQuery).toHaveBeenCalledWith('one.xqy'));
 await userEvent.click(within(focused).getByRole('button',{name:'Unfocus file one.xqy'}));
 expect(within(focused).queryByRole('button',{name:'Unfocus file one.xqy'})).not.toBeInTheDocument();
 await userEvent.click(within(focused).getByRole('button',{name:'Clear all'}));
 expect(screen.queryByRole('region',{name:'Focused queries'})).not.toBeInTheDocument();
});

it('switches the output area between results, query logs, and split view', async () => {
 await ready();
 const output=screen.getByRole('navigation',{name:'Query output views'});
 await userEvent.click(within(output).getByRole('button',{name:'Show query logs'}));
 expect(within(output).getByRole('button',{name:'Show query logs'})).toHaveAttribute('aria-pressed','true');
 expect(screen.getByRole('combobox',{name:'Query log port'})).toHaveValue('8000');
 expect(screen.getByText('No results yet')).not.toBeVisible();
 await userEvent.click(within(output).getByRole('button',{name:'Split results and logs'}));
 expect(screen.getByText('No results yet')).toBeVisible();
 expect(screen.getByRole('combobox',{name:'Query log port'})).toBeVisible();
 await userEvent.click(within(output).getByRole('button',{name:'Show query results'}));
 expect(screen.queryByRole('combobox',{name:'Query log port'})).not.toBeInTheDocument();
});


it('creates, renames and deletes scripts through the query context menu', async () => {
 vi.mocked(api.queryFolder).mockResolvedValue({root:'/queries',folders:['api'],files:[{path:'api/get.xqy',language:'xquery'}]});
 vi.mocked(api.saveScratchQuery).mockResolvedValue({path:'api/new.xqy',language:'xquery'});
 vi.mocked(api.confirmDeleteQuery).mockResolvedValue(false);
 await ready();
 fireEvent.contextMenu(screen.getByRole('button',{name:'Folder api'}),{clientX:20,clientY:30});
 await userEvent.click(screen.getByRole('menuitem',{name:'New script…'}));
 expect(screen.getByLabelText('New script file name')).toHaveValue('api/');
 await userEvent.type(screen.getByLabelText('New script file name'),'new');
 await userEvent.click(screen.getByRole('button',{name:'Create script'}));
 await waitFor(()=>expect(api.saveScratchQuery).toHaveBeenCalledWith('api/new','','xquery'));
 expect(await screen.findByRole('tab',{name:'new.xqy'})).toBeInTheDocument();
 fireEvent.contextMenu(screen.getByRole('button',{name:'XQ get.xqy'}));
 await userEvent.click(screen.getByRole('menuitem',{name:'Rename…'}));
 expect(screen.getByLabelText('New file name')).toHaveValue('api/get.xqy');
 await userEvent.click(within(screen.getByRole('dialog',{name:'Rename query'})).getByRole('button',{name:'Cancel'}));
 fireEvent.contextMenu(screen.getByRole('button',{name:'XQ get.xqy'}));
 await userEvent.click(screen.getByRole('menuitem',{name:'Delete…'}));
 await waitFor(()=>expect(api.confirmDeleteQuery).toHaveBeenCalledWith('api/get.xqy'));
 expect(api.deleteQuery).not.toHaveBeenCalled();
});

it('keeps named favorites and workspace settings separate and reopens the last session', async () => {
  localStorage.setItem('workbench.query-focus.v1', JSON.stringify({ '/queries': [{ path: 'one.xqy', kind: 'file' }] }));
  localStorage.setItem('workbench.scratch-tabs.v1', JSON.stringify([{ id: 'legacy', source: 'legacy draft', language: 'javascript' }]));
  const app = render(<App />);
  const navigation = () => within(screen.getByRole('navigation', { name: 'Workspace' }));
  const selector = (name: string) => screen.getByRole('button', { name: `Work session: ${name}` });
  const picker = () => within(screen.getByRole('dialog', { name: 'Work sessions' }));
  const queryExplorer = () => within(screen.getByRole('navigation', { name: 'Query explorer views' }));
  const restExplorer = () => within(screen.getByRole('navigation', { name: 'REST explorer views' }));
  await waitFor(() => expect(screen.getByRole('button', { name: /Run query/ })).toBeEnabled());
  expect(screen.getByLabelText('Query source')).toHaveValue('legacy draft');
  await userEvent.click(queryExplorer().getByRole('button', { name: 'Favorites' }));
  expect(screen.getByRole('button', { name: 'Unfocus file one.xqy' })).toBeVisible();

  await userEvent.click(selector('Default'));
  await userEvent.click(picker().getByRole('button', { name: 'New session' }));
  await userEvent.type(picker().getByLabelText('New session name'), 'OPS-123');
  await userEvent.click(picker().getByRole('button', { name: 'Create session' }));
  expect(await screen.findByRole('button', { name: 'Work session: OPS-123' })).toBeVisible();
  await waitFor(() => expect(screen.getByRole('button', { name: /Run query/ })).toBeEnabled());
  await userEvent.click(queryExplorer().getByRole('button', { name: 'Favorites' }));
  expect(screen.queryByRole('button', { name: 'Unfocus file one.xqy' })).toBeNull();
  await userEvent.click(queryExplorer().getByRole('button', { name: 'Queries' }));
  await userEvent.click(screen.getByRole('button', { name: 'Focus file two.sjs' }));
  await userEvent.click(screen.getByRole('button', { name: 'JS two.sjs' }));
  await waitFor(() => expect(screen.getByRole('tab', { name: 'two.sjs' })).toHaveAttribute('aria-selected', 'true'));
  fireEvent.change(screen.getByLabelText('Result format'), { target: { value: 'xml' } });

  await userEvent.click(navigation().getByRole('button', { name: 'REST' }));
  await userEvent.click(screen.getByRole('button', { name: 'Add request to favorites' }));
  await userEvent.click(restExplorer().getByRole('button', { name: 'Favorites' }));
  await userEvent.click(selector('OPS-123'));
  await userEvent.click(picker().getByRole('button', { name: 'Rename current' }));
  const name = picker().getByLabelText('Rename current session');
  await userEvent.clear(name);
  await userEvent.type(name, 'OPS-123 Investigate');
  await userEvent.click(picker().getByRole('button', { name: 'Save name' }));
  expect(selector('OPS-123 Investigate')).toBeVisible();

  await userEvent.click(selector('OPS-123 Investigate'));
  await userEvent.click(picker().getByRole('button', { name: 'Default' }));
  await waitFor(() => expect(screen.getByRole('button', { name: 'Unfocus file one.xqy' })).toBeVisible());
  expect(screen.queryByRole('button', { name: 'Unfocus file two.sjs' })).toBeNull();
  await userEvent.click(navigation().getByRole('button', { name: 'REST' }));
  expect(screen.getByRole('button', { name: 'Add request to favorites' })).toBeVisible();

  fireEvent.keyDown(window, { code: 'KeyK', key: 'K', ctrlKey: true, shiftKey: true });
  const search = picker().getByLabelText('Find sessions');
  await userEvent.type(search, 'ops-123{Enter}');
  expect(await screen.findByRole('button', { name: 'Work session: OPS-123 Investigate' })).toBeVisible();
  app.unmount();

  render(<App />);
  expect(selector('OPS-123 Investigate')).toBeVisible();
  expect(navigation().getByRole('button', { name: 'REST' })).toHaveAttribute('aria-pressed', 'true');
  expect(restExplorer().getByRole('button', { name: 'Favorites' })).toHaveAttribute('aria-pressed', 'true');
  expect(screen.getByRole('button', { name: 'Remove request from favorites' })).toBeVisible();
  await userEvent.click(navigation().getByRole('button', { name: 'Query' }));
  await waitFor(() => expect(screen.getByRole('tab', { name: 'two.sjs' })).toHaveAttribute('aria-selected', 'true'));
  expect(screen.getByLabelText('Result format')).toHaveValue('xml');
});

it('recovers a file edit when switching sessions before the query autosave runs', async () => {
  await ready();
  await userEvent.click(screen.getByRole('button', { name: 'XQ one.xqy' }));
  await waitFor(() => expect(screen.getByLabelText('Query source')).toHaveValue('1 + 1'));
  fireEvent.change(screen.getByLabelText('Query source'), { target: { value: 'keep this draft' } });
  await userEvent.click(screen.getByRole('button', { name: 'Work session: Default' }));
  await userEvent.click(screen.getByRole('button', { name: 'New session' }));
  fireEvent.change(screen.getByLabelText('New session name'), { target: { value: 'Other ticket' } });
  await userEvent.click(screen.getByRole('button', { name: 'Create session' }));
  await waitFor(() => expect(screen.getByRole('button', { name: /Run query/ })).toBeEnabled());
  expect(screen.getByLabelText('Query source')).not.toHaveValue('keep this draft');
  expect(api.saveQuery).not.toHaveBeenCalled();

  await userEvent.click(screen.getByRole('button', { name: 'Work session: Other ticket' }));
  await userEvent.click(screen.getByRole('button', { name: 'Default' }));
  await waitFor(() => expect(screen.getByLabelText('Query source')).toHaveValue('keep this draft'));
  await userEvent.click(screen.getByRole('button', { name: 'Save query' }));
  expect(api.saveQuery).toHaveBeenCalledWith('one.xqy', 'keep this draft', '1 + 1');
});
