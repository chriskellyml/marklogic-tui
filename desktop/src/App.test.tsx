import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type Workspace } from './api';
import App from './App';
import { modulesApi } from './modules-api';
vi.mock('./modules-api',async(original)=>({...await original<typeof import('./modules-api')>(),modulesApi:{cache:vi.fn(),open:vi.fn(),search:vi.fn(),checkout:vi.fn(),save:vi.fn(),push:vi.fn(),run:vi.fn(),confirmPush:vi.fn(),confirmDrop:vi.fn(),drop:vi.fn()}}));

vi.mock('./api', async (original) => ({
  ...await original<typeof import('./api')>(),
  api: {
    queryVariables: vi.fn().mockResolvedValue({variables:[],globals:{}}),
    buildInfo: vi.fn().mockResolvedValue({build:'test',executable:'/Applications/test.app'}), updateApp: vi.fn(), restartUpdatedApp: vi.fn(), loadWorkspace: vi.fn(), queryFolder: vi.fn(), openQuery: vi.fn(), saveQuery: vi.fn(), saveScratchQuery: vi.fn(),
    listDatabases: vi.fn(), executeQuery: vi.fn(), chooseFolder: vi.fn(), discardChanges: vi.fn(), renameQuery:vi.fn(),deleteQuery:vi.fn(),confirmDeleteQuery:vi.fn(),
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
  it('switches recent server and app-server pairs and flips between the last two', async () => {
    vi.mocked(api.loadWorkspace).mockResolvedValue({ ...workspace, servers: [
      { name: 'local', endpoint: '', color: '#ffcc00', appServers: [{ name: 'api', port: 8000, secure: false, content_database: 'ApiContent', modules_database: 'ApiModules' }] },
      { name: 'staging', endpoint: '', color: '#ff0000', appServers: [{ name: 'web', port: 8010, secure: false, content_database: 'WebContent', modules_database: 'WebModules' }] },
    ] });
    await ready();
    expect((document.querySelector('.environment-border') as HTMLElement).style.backgroundImage).toContain('#ffcc00');
    await userEvent.click(screen.getByTitle('Choose connection'));
    fireEvent.change(screen.getByLabelText('Database'), { target: { value: 'CustomContent' } });
    await userEvent.click(screen.getByRole('button', { name: 'OK' }));
    await userEvent.click(screen.getByTitle('Choose connection'));
    await userEvent.selectOptions(screen.getByLabelText('App server'), 'api');
    await userEvent.click(screen.getByRole('button', { name: 'OK' }));
    expect(screen.getByRole('button', { name: 'Recent environment 1: local · Profile default' })).toHaveAttribute('title', 'local · Profile default');
    await userEvent.click(screen.getByTitle('Choose connection'));
    await userEvent.selectOptions(screen.getByLabelText('Server'), 'staging');
    await userEvent.selectOptions(screen.getByLabelText('App server'), 'web');
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('local · api');
    await userEvent.click(screen.getByRole('button', { name: 'OK' }));
    expect((document.querySelector('.environment-border') as HTMLElement).style.backgroundImage).toContain('#ff0000');
    expect(screen.getByRole('button', { name: 'Recent environment 1: local · api' })).toBeEnabled();
    expect(screen.queryByRole('button', { name: /staging · Profile default/ })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Recent environment 1: local · api' }));
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('local · api');
    await userEvent.click(screen.getByTitle('Choose connection'));
    expect(screen.getByLabelText('Database')).toHaveValue('ApiContent');
    expect(screen.getByLabelText('Modules database')).toHaveValue('ApiModules');
    await userEvent.click(screen.getByRole('button', { name: 'Switch to previous environment: staging · web' }));
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('staging · web');
    await userEvent.click(screen.getByRole('button', { name: 'Switch to previous environment: local · api' }));
    expect(screen.getByTitle('Choose connection')).toHaveTextContent('local · api');
    await userEvent.click(screen.getByRole('button', { name: /Recent environment \d: local · Profile default/ }));
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
    'Connection', 'Documents', 'Query', 'Topology', 'Meters', 'CoRB', 'MLCP', 'Flux', 'Logs',
  ]);
  await userEvent.click(within(nav).getByRole('button', { name: 'MLCP' }));
  expect(screen.getByRole('heading', { name: 'MLCP' })).toBeInTheDocument();
  expect(screen.getByText('This workspace is not available yet.')).toBeInTheDocument();
  await userEvent.click(within(nav).getByRole('button', { name: 'Query' }));
  await userEvent.click(within(nav).getByRole('button', { name: 'Documents' }));
  expect(within(nav).getByRole('button', { name: 'Documents' })).toHaveAttribute('aria-pressed', 'true');
  expect(within(nav).getAllByRole('button').every(button => button.querySelector('svg[aria-hidden="true"]'))).toBe(true);
  const marker = document.querySelector('.environment-border');
  await waitFor(() => expect(marker || document.querySelector('.environment-border')).toBeInTheDocument());
  expect(document.querySelector('.titlebar .environment-border')).toBeInTheDocument();
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
