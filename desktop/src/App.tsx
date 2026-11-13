import { memo, useEffect, useMemo, useRef, useState, type Dispatch, type SetStateAction } from 'react';
import { Braces, Check, ChevronLeft, ChevronRight, Circle, FileCode2, FolderOpen, Upload, RotateCcw, Columns2,
  LoaderCircle, Play, Plus, RefreshCw, Save, Search, SquareTerminal, X } from 'lucide-react';
import { api, errorMessage, type Connection, type Language, type QueryFile, type QueryFolder,
  type Workspace } from './api';
import { LogsWorkspace } from './LogsWorkspace';
import { QueryEditor } from './QueryEditor';
import { ConnectionSelector } from './ConnectionSelector';
import { ServerWorkspace } from './ServerWorkspace';
import { createTab, moduleTabId, restoreScratchTabs, saveScratchTab, removeScratchTab, type EditorTab } from './editor-tabs';
import { SidebarResizeHandle } from './SidebarResizeHandle';
import { QueryTree } from './QueryTree';
import { ModuleBrowser } from './ModulesWorkspace';
import { modulesApi, moduleLanguage, type ModuleContext } from './modules-api';

const starters: Record<Language, string> = {
  javascript: '// Your workspace, connected to MarkLogic.\n// Run with ⌘ Enter or Ctrl Enter.\n\nxdmp.databaseName(xdmp.database());\n',
  xquery: 'xquery version "1.0-ml";\n\nxdmp:database-name(xdmp:database())\n',
  text: '',
};
const PAGE_SIZE = 50;
interface RecentEntry {id:string;label:string;kind:'query'|'module';path?:string;root?:string;context?:ModuleContext;viewedAt:number}
function readRecent():RecentEntry[]{try{const value=JSON.parse(localStorage.getItem('workbench.recent-files.v1')??'[]') as RecentEntry[];return Array.isArray(value)?value.filter(item=>item&&typeof item.id==='string'&&typeof item.viewedAt==='number'&&Date.now()-item.viewedAt<24*60*60*1000):[];}catch{return[];}}
function languageFromPath(path:string):Language{return path.endsWith('.xqy')||path.endsWith('.xquery')?'xquery':path.endsWith('.sjs')||path.endsWith('.js')?'javascript':'text';}

export default function App() {
  const [buildInfo,setBuildInfo]=useState<{build:string;executable:string}|null>(null);
  useEffect(()=>{void api.buildInfo().then(setBuildInfo).catch(()=>{});},[]);
  const [screen, setScreen] = useState<'query' | 'modules' | 'recent' | 'logs' | 'server'>('query');
  const [recent,setRecent]=useState<RecentEntry[]>(()=>readRecent());
  const [serverDirty,setServerDirty]=useState(false);
  const [serverRevision,setServerRevision]=useState(0);
  const [workspace, setWorkspace] = useState<Workspace | null>(null);
  const [folder, setFolder] = useState<QueryFolder | null>(null);
  const [serverName, setServerName] = useState('');
  const [appServer, setAppServer] = useState('');
  const [database, setDatabase] = useState('Documents');
  const [databases, setDatabases] = useState<string[]>([]);
  const [connected, setConnected] = useState(false);
  const [tabs,setTabs]=useState<EditorTab[]>(()=>restoreScratchTabs(starters.javascript));
  const [activeTabId,setActiveTabId]=useState('scratch-1');
  const tabsRef=useRef(tabs);tabsRef.current=tabs;
  const activeTab=tabs.find((tab)=>tab.id===activeTabId)??tabs[0];
  const {file,language,source,savedSource,moduleContext,moduleOriginal,modulePending,compare,result,resultContext,page}=activeTab;
  const selectedTabId=activeTab.id;
  const {setLanguage,setSource,setSavedSource,setModuleOriginal,setModulePending,setCompare,setResult,setResultContext,setPage}=useMemo(()=>{
    function setter<K extends keyof EditorTab>(key:K):Dispatch<SetStateAction<EditorTab[K]>> {
      return (value)=>setTabs((old)=>old.map((tab)=>tab.id===selectedTabId?{...tab,[key]:typeof value==='function'?(value as (current:EditorTab[K])=>EditorTab[K])(tab[key]):value}:tab));
    }
    return {setLanguage:setter('language'),setSource:setter('source'),setSavedSource:setter('savedSource'),setModuleOriginal:setter('moduleOriginal'),setModulePending:setter('modulePending'),setCompare:setter('compare'),setResult:setter('result'),setResultContext:setter('resultContext'),setPage:setter('page')};
  },[selectedTabId]);
  const [sidebarWidth,setSidebarWidth]=useState(()=>{try{return Number(window.localStorage.getItem('workbench.sidebar-width'))||450;}catch{return 450;}});
  function resizeSidebar(value:number){setSidebarWidth(Math.max(240,Math.min(value,900,window.innerWidth-380)));}
  useEffect(()=>{const fit=()=>resizeSidebar(sidebarWidth);fit();window.addEventListener('resize',fit);return()=>window.removeEventListener('resize',fit);},[sidebarWidth]);
  useEffect(()=>{const timer=setTimeout(()=>{try{window.localStorage.setItem('workbench.sidebar-width',String(sidebarWidth));}catch{/* width remains session-local */}},300);return()=>clearTimeout(timer);},[sidebarWidth]);
  const [filter, setFilter] = useState('');
  const [busy, setBusy] = useState('');
  const [running, setRunning] = useState(false);
  const [modulesDatabase, setModulesDatabase] = useState('Modules');
  const [moduleRevision, setModuleRevision] = useState(0);
  const [moduleBusy, setModuleBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('Ready when you are');
  const [pretty, setPretty] = useState(true);
  const [loading, setLoading] = useState(true);
  const [updating, setUpdating] = useState(false);
  const [updateReady, setUpdateReady] = useState(false);
  const [updateMessage, setUpdateMessage] = useState('');
  const [updateProgress, setUpdateProgress] = useState('');
  const busyRef = useRef(false);
  const dirty = source !== savedSource;
  const dirtyRef = useRef(dirty);
  dirtyRef.current = serverDirty || tabs.some((tab)=>tab.source!==tab.savedSource);
  const locked = Boolean(busy) || loading || running || moduleBusy;
  const server = workspace?.servers.find((s) => s.name === serverName);
  const connection: Connection = { server: serverName, appServer: appServer || null, database };

  useEffect(() => {
    let active = true;
    async function initialize() {
      try {
        const next = await api.loadWorkspace();
        if (!active) return;
        setWorkspace(next);
        const selected = next.servers.find((s) => s.name === next.activeServer) ?? next.servers[0];
        setServerName(selected?.name ?? '');
        const endpoint = selected?.appServers.find((s) => s.name === next.activeAppServer);
        setAppServer(endpoint?.name ?? '');
        setModulesDatabase(endpoint?.modules_database ?? 'Modules');
        setDatabase(next.activeDatabase ?? endpoint?.content_database ?? 'Documents');
        const files = await api.queryFolder(next.queryRoot);
        if (active) setFolder(files);
      } catch (e) { if (active) setError(errorMessage(e)); }
      finally { if (active) setLoading(false); }
    }
    void initialize();
    return () => { active = false; };
  }, []);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void api.protectClose(() => dirtyRef.current).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    }).catch((e) => setError(errorMessage(e)));
    return () => { disposed = true; unlisten?.(); };
  }, []);

  useEffect(() => {
    function protectEdits(event: BeforeUnloadEvent) {
      if (serverDirty || tabs.some((tab)=>tab.source!==tab.savedSource)) { event.preventDefault(); event.returnValue = ''; }
    }
    window.addEventListener('beforeunload', protectEdits);
    return () => window.removeEventListener('beforeunload', protectEdits);
  }, [tabs,serverDirty]);

  async function task(label: string, operation: () => Promise<void>) {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(label);
    setError('');
    try { await operation(); }
    catch (e) { setError(errorMessage(e)); }
    finally { setBusy(''); busyRef.current = false; }
  }

  const saving=useRef(new Map<string,Promise<void>>());
  async function saveTab(id:string):Promise<void> {
    const ongoing=saving.current.get(id);
    if(ongoing){await ongoing;return saveTab(id);}
    const tab=tabsRef.current.find((item)=>item.id===id);
    if(!tab||tab.source===tab.savedSource)return;
    const write=(async()=>{
      if(!tab.file)saveScratchTab(tab);
      else if(tab.moduleContext)await modulesApi.save(tab.moduleContext,tab.source,tab.savedSource);
      else await api.saveQuery(tab.file!.path,tab.source,tab.savedSource);
      tabsRef.current=tabsRef.current.map((item)=>item.id===id?{...item,savedSource:tab.source,modulePending:tab.moduleContext?true:item.modulePending}:item);
      setTabs((old)=>old.map((item)=>item.id===id?{...item,savedSource:tab.source,modulePending:tab.moduleContext?true:item.modulePending}:item));
      if(tab.moduleContext)setModuleRevision((n)=>n+1);
      setNotice('Saved locally');
    })();
    saving.current.set(id,write);
    try{await write;}finally{saving.current.delete(id);}
  }
  const autosave=useRef(new Map<string,{source:string;timer:ReturnType<typeof setTimeout>}>());
  const failedAutosave=useRef(new Map<string,string>());
  useEffect(()=>{
    const wanted=new Set<string>();
    for(const tab of tabs){
      if(tab.source===tab.savedSource||saving.current.has(tab.id)||failedAutosave.current.get(tab.id)===tab.source)continue;
      wanted.add(tab.id);
      if(autosave.current.get(tab.id)?.source===tab.source)continue;
      const old=autosave.current.get(tab.id);if(old)clearTimeout(old.timer);
      const timer=setTimeout(()=>{autosave.current.delete(tab.id);void saveTab(tab.id).catch((e)=>{failedAutosave.current.set(tab.id,tab.source);setError(`Autosave failed for ${tab.file?.path??'Scratch draft'}: ${errorMessage(e)}`);});},2000);
      autosave.current.set(tab.id,{source:tab.source,timer});
    }
    for(const [id,pending] of autosave.current)if(!wanted.has(id)){clearTimeout(pending.timer);autosave.current.delete(id);}
  },[tabs]);
  useEffect(()=>()=>{for(const pending of autosave.current.values())clearTimeout(pending.timer);},[]);
  function addTab(tab:EditorTab){setTabs((old)=>[...old,tab]);setActiveTabId(tab.id);}
  async function closeTab(id:string){
    if(busyRef.current||running)return;
    try{
      const tab=tabsRef.current.find((item)=>item.id===id);if(!tab)return;
      if(tab.source!==tab.savedSource){if(tab.file)await saveTab(id);else if(!await api.discardChanges())return;}
      if(!tab.file)removeScratchTab(id);
      setTabs((old)=>{const remaining=old.filter((item)=>item.id!==id);return remaining.length?remaining:[createTab(crypto.randomUUID(),starters.javascript)];});
      if(activeTabId===id){const remaining=tabsRef.current.filter((item)=>item.id!==id);setActiveTabId(remaining.at(-1)?.id??'');}
    }catch(e){setError(errorMessage(e));}
  }
  async function openFile(next:QueryFile){
    const id=`file:${folder?.root}:${next.path}`;
    if(folder)rememberRecent({id,label:next.path,kind:'query',path:next.path,root:folder.root,viewedAt:Date.now()});
    if(tabsRef.current.some((tab)=>tab.id===id)){setActiveTabId(id);return;}
    await task('Opening query',async()=>{const contents=await api.openQuery(next.path);addTab(createTab(id,contents,next,next.language));});
  }
  async function changeFolder(root?:string){
    await task('Opening folder',async()=>{
      // Backend file access is rooted in the selected folder; finish edits before switching it.
      for(const tab of tabsRef.current)if(tab.file&&tab.source!==tab.savedSource)await saveTab(tab.id);
      if(tabsRef.current.some((tab)=>!tab.file&&tab.source!==tab.savedSource)&&!await api.discardChanges())return;
      const selected=root??await api.chooseFolder();if(typeof selected!=='string')return;
      const next=await api.queryFolder(selected);setFolder(next);setFilter('');
      const tab=createTab(crypto.randomUUID(),starters.javascript);setTabs([tab]);setActiveTabId(tab.id);
    });
  }
  async function openModule(context:ModuleContext,_content:{content:string;original:string},pending:boolean){
    rememberRecent({id:`recent:${moduleTabId(context)}`,label:context.uri,kind:'module',context,viewedAt:Date.now()});
    const id=moduleTabId(context);
    if(tabsRef.current.some((tab)=>tab.id===id)){setActiveTabId(id);return;}
    await task('Opening module',async()=>{
      const content=await modulesApi.open(context);
      addTab({...createTab(id,content.content,{path:context.uri,language:moduleLanguage(context.uri)},moduleLanguage(context.uri)),moduleContext:context,moduleOriginal:content.original,modulePending:pending});
    });
  }
  function rememberRecent(entry:RecentEntry){setRecent((old)=>{const next=[entry,...old.filter((item)=>item.id!==entry.id)].filter((item)=>Date.now()-item.viewedAt<24*60*60*1000).slice(0,100);try{localStorage.setItem('workbench.recent-files.v1',JSON.stringify(next));}catch{/* Recent history remains available for this session. */}return next;});}
  async function openRecent(entry:RecentEntry){
    if(entry.kind==='module'&&entry.context){const context=entry.context;if(tabsRef.current.some(tab=>tab.id===moduleTabId(context))){setActiveTabId(moduleTabId(context));setScreen('query');return;}await task('Opening module',async()=>{const content=await modulesApi.open(context);addTab({...createTab(moduleTabId(context),content.content,{path:context.uri,language:moduleLanguage(context.uri)},moduleLanguage(context.uri)),moduleContext:context,moduleOriginal:content.original,modulePending:content.content!==content.original});setScreen('query');});return;}
    if(entry.kind==='query'&&entry.path&&entry.root){const path=entry.path,root=entry.root,id=`file:${root}:${path}`;if(tabsRef.current.some(tab=>tab.id===id)){setActiveTabId(id);setScreen('query');return;}await task('Opening query',async()=>{if(folder?.root!==root){const next=await api.queryFolder(root);setFolder(next);}const contents=await api.openQuery(path);const next=folder?.files.find(item=>item.path===path);const language=next?.language??languageFromPath(path);addTab(createTab(id,contents,{path,language},language));setScreen('query');});}
  }
  async function syncModule() {
    if (!moduleContext || locked) return;
    await task('Syncing module', async()=>{
      if (!await modulesApi.confirmPush(moduleContext)) return;
      await saveTab(activeTab.id);
      await modulesApi.push(moduleContext,source);
      setModuleOriginal(source); setModulePending(false); setModuleRevision((n)=>n+1); setNotice('Module synced to server');
    });
  }
  async function dropModule() {
    if (!moduleContext || locked) return;
    await task('Restoring server copy',async()=>{
      if (!await modulesApi.confirmDrop(moduleContext)) return;
      const pending=autosave.current.get(activeTab.id);if(pending){clearTimeout(pending.timer);autosave.current.delete(activeTab.id);}
      await saving.current.get(activeTab.id);
      const latest=tabsRef.current.find((tab)=>tab.id===activeTab.id)!;
      const remote=await modulesApi.drop(moduleContext,latest.savedSource);
      setSource(remote.content);setSavedSource(remote.content);setModuleOriginal(remote.original);setModulePending(false);setCompare(false);setModuleRevision((n)=>n+1);setNotice('Local module edits dropped; server copy restored');
    });
  }
  async function save(){if(!file||!dirty||locked)return;await task('Saving query',()=>saveTab(activeTab.id));}

  async function run() {
    if (moduleContext || !server || !source.trim() || language === 'text' || locked || busyRef.current) return;
    busyRef.current = true; setRunning(true); setError(''); setResult(null); setPage(0);
    const context = `${serverName} / ${database} · ${file?.path ?? 'Scratch query'}`;
    setResultContext(context);
    try {
      const next = await api.executeQuery(connection, source, language, file?.path ?? null);
      setResult(next); setNotice(`Completed · ${next.items.length} result${next.items.length === 1 ? '' : 's'}`);
    } catch (e) { setError(errorMessage(e)); setNotice('Query failed'); }
    finally { setRunning(false); busyRef.current = false; }
  }

  const actions = useRef({ run, save });
  actions.current = { run, save };
  useEffect(() => {
    function keyboard(event: KeyboardEvent) {
      if (screen !== 'query' || !(event.metaKey || event.ctrlKey)) return;
      if (event.key === 'Enter') { event.preventDefault(); void actions.current.run(); }
      if (event.key.toLowerCase() === 's') { event.preventDefault(); void actions.current.save(); }
    }
    window.addEventListener('keydown', keyboard);
    return () => window.removeEventListener('keydown', keyboard);
  }, [screen]);

  function selectServer(name: string) {
    setServerName(name); setAppServer(''); setDatabases([]); setConnected(false);
    setDatabase('Documents'); setModulesDatabase('Modules');
  }

  function selectAppServer(name: string) {
    setAppServer(name); setDatabases([]); setConnected(false);
    setDatabase(server?.appServers.find((s) => s.name === name)?.content_database ?? 'Documents');
    setModulesDatabase(server?.appServers.find((s) => s.name === name)?.modules_database ?? 'Modules');
  }

  const visibleModule = moduleContext && moduleContext.connection.server===connection.server && moduleContext.connection.appServer===connection.appServer && moduleContext.database===modulesDatabase;
  const files = folder?.files.filter((item) => item.path.toLowerCase().includes(filter.toLowerCase())) ?? [];
  const pageCount = Math.max(1, Math.ceil((result?.items.length ?? 0) / PAGE_SIZE));

  async function updateApp() {
    setUpdating(true);
    try {
      if (!updateReady) {
        setUpdateMessage('Building the app from this checkout…');
        setUpdateProgress('Starting build…');
        await api.updateApp(setUpdateProgress);
        setUpdateProgress('');
        setUpdateReady(true);
      }
      if (dirtyRef.current && !await api.discardChanges()) {
        setUpdateMessage('Update installed. Save your query, then select Restart updated app.');
        return;
      }
      setUpdateMessage('Starting the updated app…');
      await api.restartUpdatedApp();
    } catch (error) { setUpdateProgress(''); setUpdateMessage(`Update failed: ${errorMessage(error)}`); }
    finally { setUpdating(false); }
  }

  return <div className={`app-shell ${screen === 'logs' ? 'logs-active' : 'query-active'}`}>
    <a className="skip-link" href="#query-editor">Skip to query editor</a>
    <header className="titlebar">
      <div className="brand-mark"><Braces size={21} /></div>
      <span className="brand">marklogic<span className="brand-secondary"> / workbench</span></span>
      <span className="preview-badge">DESKTOP PREVIEW</span>
      <nav className="workspace-navigation" aria-label="Workspace"><button aria-pressed={screen === 'query'} onClick={() => setScreen('query')}>Queries</button><button aria-pressed={screen === 'modules'} onClick={() => setScreen('modules')}>Modules</button><button aria-pressed={screen === 'recent'} onClick={() => setScreen('recent')}>Recent</button><button aria-pressed={screen === 'logs'} onClick={() => setScreen('logs')}>Logs</button><button aria-pressed={screen === 'server'} onClick={() => setScreen('server')}>Server</button></nav>
      <div className="titlebar-right">{buildInfo&&<span title={`Running: ${buildInfo.executable}`} aria-label={`Build ${buildInfo.build}`}>Build {buildInfo.build}</span>}<span className="status-dot" /> Local workspace
        <button className="app-update-button" disabled={updating} onClick={() => void updateApp()} title="Build and install the app from this local checkout">{updating ? <LoaderCircle className="spin" size={13} /> : <RefreshCw size={13} />} {updating ? 'Updating…' : updateReady ? 'Restart updated app' : 'Update app'}</button>
      <ConnectionSelector servers={workspace?.servers??[]} connection={connection} disabled={locked} onServer={selectServer} onAppServer={selectAppServer}>
          <label className="database-label">DATABASE<input aria-label="Database" list="databases" value={database}
            disabled={locked || !server} onChange={(e) => setDatabase(e.target.value)} />
            <datalist id="databases">{databases.map((name) => <option key={name} value={name} />)}</datalist>
          </label>
          <button className="connect-button" disabled={locked || !server || !database.trim()}
            onClick={() => void task('Connecting', async () => {
              setConnected(false);
              const next = await api.listDatabases(connection);
              setDatabases(next); setConnected(true); setNotice('Connected · databases refreshed');
            })}>{busy === 'Connecting' ? <LoaderCircle className="spin" size={14} /> : connected ? <Check size={14} /> : <RefreshCw size={14} />}
            {connected ? 'Connected' : 'Connect'}</button>
          <label>MODULES DATABASE<input aria-label="Modules database" value={modulesDatabase} disabled={locked} onChange={(e)=>setModulesDatabase(e.target.value)} /></label>
      </ConnectionSelector>
      </div>
    </header>
    {updateMessage && <div className="app-update-message" role="status"><div className="app-update-content"><span>{updateMessage}</span>{updating && <><progress aria-label="Application update progress" /><span className="app-update-step">{updateProgress}</span></>}</div>{!updating && <button aria-label="Dismiss update message" onClick={() => setUpdateMessage('')}><X size={14} /></button>}</div>}


    <LogsWorkspace key={serverRevision} active={screen === 'logs'} servers={workspace?.servers ?? []} connection={connection} disabled={locked}
      onServer={selectServer} onAppServer={selectAppServer} />

    <ServerWorkspace active={screen === 'server'} workspace={workspace} selectedServer={serverName} onDirtyChange={setServerDirty} onSaved={async(name)=>{
      const next=await api.loadWorkspace(); setWorkspace(next);
      if(name===serverName || !serverName){setServerName(name);if(!next.servers.find(s=>s.name===name)?.appServers.some(s=>s.name===appServer))setAppServer('');setDatabases([]);setConnected(false);setServerRevision(n=>n+1);}
    }} />

    <div className="workspace-layout" style={{gridTemplateColumns:`${sidebarWidth}px 6px minmax(0,1fr)`}} hidden={screen !== 'query'}>
      <aside className="sidebar" aria-label="Query files">
        <div className="source-heading"><strong>FILES</strong><span>{folder?.files.length??0}</span></div>
        <button className="folder-button" onClick={() => void changeFolder()} disabled={locked || !workspace}>
          <FolderOpen size={16} /><span>{folder ? basename(folder.root) : 'Open a folder'}</span><Plus size={14} />
        </button>
        <p className="folder-path" title={folder?.root}>{folder?.root ?? 'Your queries live on your machine'}</p>
        <label className="file-search"><Search size={14} /><input aria-label="Filter query files" placeholder="Find a query…"
          value={filter} onChange={(e) => setFilter(e.target.value)} /></label>
        <div className="file-list fused-files">
          <QueryTree key={folder?.root} items={folder?.files ?? []} folders={folder?.folders} filter={filter} selected={moduleContext?undefined:file?.path} disabled={locked} onOpen={(item) => void openFile(folder!.files.find((f) => f.path === item.path)!)} />
          {!files.length && <div className="empty-files">{loading ? 'Loading your workspace…' : filter ? 'No matching queries' : 'Open a query folder, or start with a scratch query.'}</div>}
        </div>
        <button className="new-query" disabled={locked} onClick={()=>addTab(createTab(crypto.randomUUID(),starters.javascript))}><Plus size={15}/> New scratch query</button>
        {Boolean(workspace?.trackedFolders.length) && <div className="tracked-folders">
          <label className="section-label" htmlFor="tracked-folder">SAVED FOLDERS</label>
          <select id="tracked-folder" value="" disabled={locked} onChange={(e) => void changeFolder(e.target.value)}>
            <option value="">Choose a folder…</option>
            {workspace?.trackedFolders.map((root) => <option key={root} value={root}>{root}</option>)}
          </select>
        </div>}
      </aside>

      <SidebarResizeHandle width={sidebarWidth} onChange={resizeSidebar}/>
      <main className="main-workspace">


        {workspace && !workspace.servers.length && <div className="setup-note">Add a server in the Server tab. Profiles are saved in <code>{workspace.configPath}</code>.</div>}
        {error && <div className="error-banner" role="alert"><pre>{error}</pre><button aria-label="Dismiss error" onClick={() => setError('')}><X size={16} /></button></div>}

        <div className="editor-tabs" role="tablist" aria-label="Open files">{tabs.map((tab)=><div key={tab.id} className={`editor-tab-item ${tab.id===activeTab.id?'active':''} ${tab.moduleContext&&(tab.modulePending||tab.source!==tab.savedSource)?'pending':''}`}>
          <button role="tab" aria-selected={tab.id===activeTab.id} aria-controls="query-editor" title={tab.moduleContext?`${tab.moduleContext.connection.server} / ${tab.moduleContext.database} / ${tab.file?.path}`:tab.file?.path??'Scratch query'} onClick={()=>setActiveTabId(tab.id)}>{tab.moduleContext?'M · ':''}{tab.file?basename(tab.file.path):'Scratch'}{tab.source!==tab.savedSource?' •':tab.moduleContext&&tab.modulePending?' !':''}</button>
          <button aria-label={`Close ${tab.file?.path??'scratch'}`} title="Close tab" disabled={Boolean(busy)||running} onClick={()=>void closeTab(tab.id)}><X size={12}/></button>
        </div>)}</div>
        {moduleContext && <div className={`module-sync-state ${modulePending||dirty?'pending':'clean'}`}>
          <span><strong>{modulePending||dirty?'UNSYNCED MODULE — sync or drop local changes':'Module synced'}</strong><small>{moduleContext.connection.server} · {moduleContext.database} · {moduleContext.uri}</small></span>
          <button aria-label="Sync module to server" title="Save current edits and sync to server" disabled={locked} onClick={()=>void syncModule()}><Upload size={16}/></button>
          <button aria-label="Drop module changes" title="Drop local edits and download server copy" disabled={locked} onClick={()=>void dropModule()}><RotateCcw size={16}/></button>
        </div>}
        <section className="editor-panel" id="query-editor" aria-label="Query editor">
          <div className="panel-toolbar"><div className="editor-tab"><FileCode2 size={15} />{file ? basename(file.path) : 'Untitled query'}
            {dirty && <span className="unsaved-dot" title="Unsaved changes" aria-label="Unsaved changes" />}</div>
            <div className="toolbar-actions"><select className="language-select" aria-label="Query language" value={language}
              disabled={locked || Boolean(file)} onChange={(e) => {
                const next = e.target.value as Language;
                if (source === starters[language]) {setSource(starters[next]);setSavedSource(starters[next]);setLanguage(next);}
                else setLanguage(next);
              }}>
              <option value="javascript">JavaScript</option><option value="xquery">XQuery</option><option value="text" disabled>Text</option>
            </select>{moduleContext&&<button className="icon-button" aria-label="Compare module" title="Compare with server snapshot" aria-pressed={compare} disabled={locked} onClick={()=>setCompare(!compare)}><Columns2 size={16}/></button>}<button className="icon-button" aria-label={moduleContext?'Save module locally':'Save query'} title={moduleContext?'Save locally; still needs server sync (⌘S / Ctrl+S)':'Save query (⌘S / Ctrl+S)'}
              disabled={!file || !dirty || locked} onClick={() => void save()}><Save size={16} /></button>
              {!moduleContext&&<><span className="toolbar-divider" /><button className="run-button" disabled={!server || locked || language === 'text' || !source.trim() || !database.trim()}
                onClick={() => void run()}>{running ? <LoaderCircle className="spin" size={14} /> : <Play size={13} fill="currentColor" />}
                {running ? 'Running…' : 'Run query'}<kbd>⌘ ↵</kbd></button></>}</div>
          </div>
          {moduleContext&&compare&&<div className="module-comparison"><div><strong>Server snapshot</strong><pre>{moduleOriginal}</pre></div><div><strong>Current edits</strong><pre>{source}</pre></div></div>}
          <div className="editor-body"><QueryEditor key={activeTab.id} source={source} language={language} onChange={setSource} disabled={locked} /></div>
          <div className="editor-footer"><span>{file ? file.path : 'Scratch draft · autosaved on this device'}</span>
            <span>{source.split('\n').length} lines <span className="footer-separator">/</span> UTF-8</span></div>
        </section>

        {!moduleContext&&<section className="results-panel" aria-label="Query results" aria-busy={running}>
          <div className="panel-toolbar"><div className="results-title"><SquareTerminal size={16} /> Results
            {result && <span className="result-count">{result.items.length}</span>}</div>
            <div className="result-options">{result && <span className="timing">{result.elapsedMs.toLocaleString()} ms</span>}
              <button className={`format-button ${pretty ? 'active' : ''}`} aria-pressed={pretty} onClick={() => setPretty(!pretty)}><Braces size={14} /> Pretty JSON</button></div>
          </div>
          {result && <div className="result-context">{resultContext}</div>}
          <div className="results-body">
            {running ? <div className="results-empty"><LoaderCircle className="spin" size={24} /><strong>Running your query</strong><p>Waiting for MarkLogic to respond.</p></div>
              : result?.items.length ? <ResultItems items={result.items} page={page} pretty={pretty} />
              : <div className="results-empty"><div className="empty-result-icon"><SquareTerminal size={23} /></div>
                <strong>{result ? 'Query completed with no output' : 'No results yet'}</strong>
                <p>{result ? 'MarkLogic returned an empty result sequence.' : '⌘ Enter to run'}</p>
                {!result && <span className="shortcut-hint"><kbd>⌘</kbd> / <kbd>Ctrl</kbd> + <kbd>Enter</kbd> to run</span>}</div>}
          </div>
          {pageCount > 1 && <div className="pagination"><span>Page {page + 1} of {pageCount}</span>
            <button aria-label="Previous results" disabled={page === 0} onClick={() => setPage(page - 1)}><ChevronLeft size={16} /></button>
            <button aria-label="Next results" disabled={page + 1 >= pageCount} onClick={() => setPage(page + 1)}><ChevronRight size={16} /></button></div>}
        </section>}
        <footer className="workspace-status" role="status"><span><Circle size={7} fill="currentColor" />{busy || (running ? 'Query running' : notice)}</span>
          <span>{server?.endpoint ?? 'No server selected'}</span></footer>
      </main>
    </div>
    <section className="standalone-workspace" aria-label="Module browser" hidden={screen!=='modules'}><ModuleBrowser connection={connection} database={modulesDatabase} selectedUri={visibleModule?moduleContext?.uri:undefined} selectedPending={Boolean(visibleModule&&(modulePending||dirty))} revision={moduleRevision} disabled={Boolean(busy)||running||loading} onOpen={openModule} onBusy={setModuleBusy} onError={setError}/></section>
    <section className="recent-workspace" aria-label="Recently viewed files" hidden={screen!=='recent'}><div className="recent-heading"><strong>VIEWED IN THE LAST 24 HOURS</strong><span>{recent.length} files</span></div>{recent.filter(item=>Date.now()-item.viewedAt<24*60*60*1000).map(item=><button className="recent-file" key={item.id} onClick={()=>void openRecent(item)}><span>{item.kind==='module'?'MODULE':'QUERY'}</span><strong>{item.label}</strong><small>{new Date(item.viewedAt).toLocaleString()}</small></button>)}{!recent.length&&<p>No files viewed in the last 24 hours.</p>}</section>
  </div>;
}

const ResultItems = memo(function ResultItems({ items, page, pretty }: { items: string[]; page: number; pretty: boolean }) {
  return items.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE).map((item, index) =>
    <div className="result-item" key={page * PAGE_SIZE + index}>
      <span className="result-index">{page * PAGE_SIZE + index + 1}</span><pre>{formatResult(item, pretty)}</pre>
    </div>);
});

function basename(path: string) { return path.split(/[\\/]/).pop() || path; }
function formatResult(item: string, pretty: boolean) {
  if (pretty) { try { return JSON.stringify(JSON.parse(item), null, 2); } catch { /* XML and text stay intact. */ } }
  return item;
}
