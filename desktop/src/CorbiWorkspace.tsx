import { useEffect, useRef, useState } from 'react';
import { Braces, Check, ChevronRight, Clock3, FileText, FolderOpen, Layers3, LoaderCircle, Play, Plus, Pencil, RefreshCw, Save, Square } from 'lucide-react';
import { api, errorMessage, type Connection } from './api';
import { corbiApi, targetLabel, type CorbiCatalog, type CorbiProject, type CorbiRun, type RunEntry } from './corbi-api';
import { CorbiScriptTree } from './CorbiScriptTree';
import { CorbiRunHistory } from './CorbiRunHistory';
import { QueryEditor } from './QueryEditor';
import { useSessionKey, useSessionState, useWorkbenchSession } from './workbench-sessions';
import './corbi.css';

function storedRoot(storageKey: string) {
  try { return localStorage.getItem(storageKey); } catch { return null; }
}

interface TargetProps { connection: Connection; modulesDatabase: string }
export function CorbiWorkspace({ active, onDirty, connection, modulesDatabase }: TargetProps & { active: boolean; onDirty: (dirty: boolean) => void }) {
  const storageKey = useSessionKey('workbench.corbi.data-folder');
  const reportStorage = useWorkbenchSession()?.reportStorage;
  const [catalog, setCatalog] = useState<CorbiCatalog | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);
  const initialized = useRef(false);
  const [revision, setRevision] = useState(0);
  useEffect(() => { onDirty(dirty); }, [dirty, onDirty]);

  useEffect(() => {
    if (!active || initialized.current) return;
    let live = true;
    setLoading(true);
    void (async () => {
      const saved = storedRoot(storageKey);
      try { return await corbiApi.open(saved); }
      catch (e) {
        if (!saved) throw e;
        const data = await corbiApi.open(null);
        if (live) setError(`Could not reopen the previous folder. Your Workbench jobs are available. ${errorMessage(e)}`);
        return data;
      }
    })().then(data => { if (live) setCatalog(data); })
      .catch(e => { if (live) setError(errorMessage(e)); })
      .finally(() => { if (live) { setLoading(false); initialized.current = true; } });
    return () => { live = false; };
  }, [active, revision]);

  async function openFolder(refresh = false) {
    setLoading(true);
    setError('');
    try {
      const path = refresh ? catalog?.root : await corbiApi.choose();
      if (!path || (dirty && !await api.discardChanges())) return;
      const next = await corbiApi.open(path);
      if (next) {
        setCatalog(next); setDirty(false); setBusy(false); setRevision(r => r + 1);
        try { localStorage.setItem(storageKey, next.root); reportStorage?.(storageKey, false); } catch { reportStorage?.(storageKey, true); }
      }
    } catch (e) { setError(errorMessage(e)); }
    finally { setLoading(false); }
  }

  if (!active && !catalog) return null;
  return <main hidden={!active} className="corbi-workspace" aria-label="CoRBi workspace">
    {error && <div className="corbi-error" role="alert">{error}</div>}
    {catalog ? <RepositoryWorkspace key={catalog.root + ':' + revision} catalog={catalog} active={active} connection={connection} modulesDatabase={modulesDatabase}
      onDirty={setDirty} onBusy={setBusy} loadingFolder={loading} openFolder={openFolder} busy={busy}/>
      : <div className="corbi-welcome" role="status">
        {loading ? <><LoaderCircle className="spin" size={24}/><p>Opening your jobs…</p></>
          : <><p>Your jobs could not be opened.</p><button onClick={() => { initialized.current = false; setRevision(n => n + 1); }}>Retry</button>
            <button onClick={() => void openFolder()}><FolderOpen size={16}/>Open folder</button></>}
      </div>}
  </main>;
}

interface WorkspaceProps extends TargetProps {
  catalog: CorbiCatalog; active: boolean; onDirty: (value: boolean) => void; onBusy: (value: boolean) => void;
  loadingFolder: boolean; openFolder: (refresh?: boolean) => Promise<void>; busy: boolean;
}
function firstFile(project?: CorbiProject) { return project?.jobs[0] ?? project?.scripts[0] ?? ''; }

function RepositoryWorkspace({ catalog, active, onDirty, onBusy, loadingFolder, openFolder, busy, connection, modulesDatabase }: WorkspaceProps) {
  const [data, setData] = useState(catalog);
  const { root, projects } = data;
  const stateKey = `corbi:${root}`;
  const [storedProject, setProjectName] = useSessionState(stateKey + ':project', projects[0]?.name ?? '');
  const project = projects.find(item => item.name === storedProject) ?? projects[0];
  const projectName = project?.name ?? '';
  const [storedFile, setFile] = useSessionState(stateKey + ':file', () => firstFile(project));
  const file = project && [...project.jobs, ...project.scripts].includes(storedFile) ? storedFile : firstFile(project);
  const [storedJob, setSelectedJob] = useSessionState(stateKey + ':job', project?.jobs[0] ?? '');
  const selectedJob = project?.jobs.includes(storedJob) ? storedJob : project?.jobs[0] ?? '';
  const [draft, setDraft] = useSessionState<{ project: string; file: string; source: string; baseline: string } | null>(stateKey + ':draft', null);
  const draftRef = useRef(draft); draftRef.current = draft;
  const [jobDefinition, setJobDefinition] = useState('');
  const [creating, setCreating] = useState(false);
  const [creatingProject, setCreatingProject] = useState(false);
  const [projectInput, setProjectInput] = useState('');
  const [newProject, setNewProject] = useState(projectName || 'My jobs');
  const [newName, setNewName] = useState('');
  const [renamePath, setRenamePath] = useState('');
  const [renameName, setRenameName] = useState('');
  const [sourceRevision, setSourceRevision] = useState(0);
  const [starting, setStarting] = useState(false);
  const env = connection.server;
  const [view, setView] = useSessionState<'source' | 'history'>(stateKey + ':view', 'source');
  const [original, setOriginal] = useState<string | null>(null);
  const [text, setText] = useState('');
  const [limit, setLimit] = useSessionState(stateKey + ':limit', '10');
  const [run, setRun] = useState<CorbiRun | null>(null);
  const [historyRevision, setHistoryRevision] = useState(0);
  const [historyEntry, setHistoryEntry] = useState<RunEntry | null>(null);
  const [error, setError] = useState('');
  const [saved, setSaved] = useState(false);
  const [pending, setPending] = useState(false);
  const [statusError, setStatusError] = useState('');
  const [pollRevision, setPollRevision] = useState(0);
  const dirty = original !== null && text !== original;
  const running = run?.status === 'running';
  const locked = loadingFolder || pending;
  const job = project?.jobs.includes(selectedJob);
  const enabled = data.permissions[projectName]?.[selectedJob]?.[env] !== false;
  const promotable = run && !run.wet && run.status === 'completed' && run.project === projectName && run.job === selectedJob && run.connection.server === env && run.connection.appServer === connection.appServer && run.connection.database === connection.database && run.modulesDatabase === modulesDatabase;
  const limitValid = !limit || (/^\d+$/.test(limit) && Number(limit) > 0 && Number(limit) <= 4294967295);
  const canRun = job && env && connection.database.trim() && original !== null && enabled && !running && !locked && !dirty;
  useEffect(() => { onDirty(dirty); }, [dirty, onDirty]);
  useEffect(() => { onBusy(running || pending); }, [running, pending, onBusy]);

  useEffect(() => {
    let live = true;
    setOriginal(null); setText(''); setSaved(false); setError('');
    if (file) void corbiApi.source(root, projectName, file).then(value => {
      if (live) {
        const restored = draftRef.current;
        const editing = restored?.project === projectName && restored.file === file && restored.source !== value;
        setText(editing ? restored.source : value);
        setOriginal(editing ? restored.baseline : value);
        if (!editing && restored?.project === projectName && restored.file === file) setDraft(null);
        if (file.endsWith('.job')) setJobDefinition(value);
      }
    }).catch(e => { if (live) setError(errorMessage(e)); });
    return () => { live = false; };
  }, [root, projectName, file, sourceRevision]);

  useEffect(() => {
    if (!active) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    const check = async () => {
      try {
        const next = await corbiApi.status(root);
        if (!live) return;
        setRun(next); setStatusError('');
        if (next?.status === 'running') timer = setTimeout(check, 900);
      } catch (e) { if (live) setStatusError(errorMessage(e)); }
    };
    void check();
    return () => { live = false; clearTimeout(timer); };
  }, [active, root, pollRevision]);

  async function saveCurrent(): Promise<boolean> {
    if (!dirty || original === null) return true;
    try {
      await corbiApi.save(root, projectName, file, text, original);
      setDraft(null); setOriginal(text); setSaved(true);
      if (file.endsWith('.job')) setJobDefinition(text);
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    }
  }

  async function select(nextProject: CorbiProject, nextFile: string) {
    if (locked) return;
    if (nextProject.name !== projectName || nextFile !== file) {
      setPending(true); setError('');
      try {
        if (!await saveCurrent()) return;
        setDraft(null); setOriginal(null); setText(''); setFile(nextFile);
        setProjectName(nextProject.name); setHistoryEntry(null);
        if (nextProject.jobs.includes(nextFile)) setSelectedJob(nextFile);
        else if (nextProject.name !== projectName) setSelectedJob(nextProject.jobs[0] ?? '');
      } finally { setPending(false); }
    }
    setView('source');
  }

  async function save() {
    if (!dirty || original === null || locked) return;
    setPending(true); setError('');
    try { await saveCurrent(); }
    finally { setPending(false); }
  }

  async function createProject(event: React.FormEvent) {
    event.preventDefault();
    const name = projectInput.trim();
    if (locked || running || !name) return;
    setPending(true); setError('');
    try {
      if (!await saveCurrent()) return;
      const next = await corbiApi.createProject(root, name);
      setDraft(null); setData(next); setProjectName(name); setFile(''); setSelectedJob('');
      setOriginal(null); setText(''); setJobDefinition(''); setHistoryEntry(null);
      setView('source'); setCreatingProject(false); setProjectInput('');
    } catch (e) { setError(errorMessage(e)); }
    finally { setPending(false); }
  }

  async function start(wet: boolean) {
    if (!canRun || (wet ? !promotable : !limitValid)) return;
    setPending(true); setStarting(true); setError('');
    try {
      if (wet && run && !await corbiApi.confirmWet(run)) return;
      const next = await corbiApi.start(root, { project: projectName, job: selectedJob, connection, modulesDatabase,
        limit: wet || !limit ? null : Number(limit), dryRunId: wet && run ? run.id : null });
      setRun(next); setHistoryRevision(n => n + 1); setHistoryEntry({ id: next.id, environment: next.historyEnv }); setView('history');
    } catch (e) { setError(errorMessage(e)); }
    finally { setPollRevision(r => r + 1); setPending(false); setStarting(false); }
  }
  async function stop() {
    if (!run || locked) return;
    setPending(true); setError('');
    try { await corbiApi.stop(root, run.id); }
    catch (e) { setError(errorMessage(e)); }
    finally { setPollRevision(r => r + 1); setPending(false); }
  }

  async function createJob(event: React.FormEvent) {
    event.preventDefault();
    if (locked || running || !newName.trim() || !newProject.trim()) return;
    setPending(true); setError('');
    try {
      if (!await saveCurrent()) return;
      const next = await corbiApi.createJob(root, newProject.trim(), newName.trim());
      setDraft(null); setData(next); setOriginal(null); setText(''); setProjectName(newProject.trim());
      setSelectedJob(newName.trim() + '.job'); setFile(newName.trim() + '.job');
      setView('source'); setCreating(false); setNewName('');
    } catch (e) { setError(errorMessage(e)); }
    finally { setPending(false); }
  }

  function beginRename(path: string) {
    setRenamePath(path); setRenameName(path.split('/').pop() ?? '');
  }
  async function rename(event: React.FormEvent) {
    event.preventDefault();
    if (locked || running || !renameName.trim()) return;
    setPending(true); setError('');
    const destination = renamePath.split('/').slice(0, -1).concat(renameName.trim()).join('/');
    try {
      if (!await saveCurrent()) return;
      const next = await corbiApi.rename(root, projectName, renamePath, renameName.trim());
      const remap = (value: string) => value === renamePath || value.startsWith(renamePath + '/') ? destination + value.slice(renamePath.length) : value;
      setDraft(null); setData(next); setFile(remap(file)); setSelectedJob(remap(selectedJob));
      setOriginal(null); setText(''); setSourceRevision(n => n + 1); setRenamePath('');
      const currentJob = remap(selectedJob);
      if (currentJob) setJobDefinition(await corbiApi.source(root, projectName, currentJob));
    } catch (e) { setError(errorMessage(e)); }
    finally { setPending(false); }
  }

  function modulePath(property: string) {
    const definition = file === selectedJob ? text : jobDefinition;
    const value = definition.split('\n').find(line => new RegExp('^\\s*' + property + '\\s*=').test(line))?.split('=').slice(1).join('=').trim().split('|')[0];
    return value && project?.scripts.includes(value) ? value : null;
  }
  const collector = modulePath('URIS[-_]MODULE');
  const processor = modulePath('PROCESS[-_]MODULE');

  return <div className="corbi-layout">
    <aside className="corbi-sidebar" aria-label="CoRBi project explorer">
      <div className="corbi-section-heading"><Layers3 size={17}/><strong>CoRBi</strong><span>Batch runner</span></div>
      <div className="corbi-project-actions">
        <button className="corbi-new-job" disabled={locked || running} onClick={() => { setCreatingProject(v => !v); setCreating(false); }}><Plus size={15}/>New project</button>
        <button title="Refresh projects" aria-label="Refresh data folder" disabled={busy || loadingFolder} onClick={() => void openFolder(true)}><RefreshCw size={14}/></button>
      </div>
      {creatingProject && <form className="corbi-create-form" onSubmit={event => void createProject(event)}>
        <label className="corbi-field">Project name<input aria-label="Project name" autoFocus required value={projectInput} onChange={e => setProjectInput(e.target.value)}/></label>
        <div><button type="button" onClick={() => setCreatingProject(false)}>Cancel</button><button className="run-button" type="submit" disabled={locked || !projectInput.trim()}>Create project</button></div>
      </form>}
      <button className="corbi-new-job" disabled={locked || running} onClick={() => { setCreating(v => !v); setCreatingProject(false); setNewProject(projectName || 'My jobs'); }}><Plus size={15}/>New job</button>
      {creating && <form className="corbi-create-form" onSubmit={event => void createJob(event)}>
        <label className="corbi-field">Project<input aria-label="New job project" list="corbi-projects" value={newProject} required onChange={e => setNewProject(e.target.value)}/></label>
        <datalist id="corbi-projects">{projects.map(p => <option key={p.name} value={p.name}/>)}</datalist>
        <label className="corbi-field">Job name<input aria-label="New job name" autoFocus placeholder="update-documents" value={newName} required onChange={e => setNewName(e.target.value)}/></label>
        <div><button type="button" onClick={() => setCreating(false)}>Cancel</button><button type="submit" className="run-button" disabled={locked || !newName.trim() || !newProject.trim()}>Create job</button></div>
      </form>}
      <label className="corbi-field">Project<select value={projectName} disabled={locked || !projects.length}
        onChange={e => { const p = projects.find(p => p.name === e.target.value); if (p) void select(p, firstFile(p)); }}>
        {projects.map(p => <option key={p.name}>{p.name}</option>)}</select></label>
      {renamePath && <form className="corbi-create-form" onSubmit={event => void rename(event)}>
        <label className="corbi-field">Rename {renamePath}<input aria-label="New name" value={renameName} autoFocus required onChange={e => setRenameName(e.target.value)}/></label>
        <p className="corbi-note">Updates module paths in job files.</p>
        <div><button type="button" onClick={() => setRenamePath('')}>Cancel</button><button className="run-button" type="submit" disabled={locked || !renameName.trim() || renameName === renamePath.split('/').pop()}>Rename</button></div>
      </form>}
      <div className="corbi-files">
        {project && <>
          <details className="corbi-tree-section" open><summary>Jobs <span>{project.jobs.length}</span></summary>
            {project.jobs.map(path => <div className="corbi-tree-row" key={path}><button className="file-button" aria-pressed={selectedJob === path && view === 'source'} disabled={locked} onClick={() => void select(project, path)}><FileText size={14}/><span>{path}</span></button>
              <button className="corbi-rename-button" aria-label={`Rename ${path}`} title="Rename" disabled={locked || running} onClick={() => beginRename(path)}><Pencil size={12}/></button></div>)}
          </details>
          <details className="corbi-tree-section" open><summary>Scripts <span>{project.scripts.length}</span></summary>
            <CorbiScriptTree key={projectName} paths={project.scripts} selected={view === 'source' ? file : ''} disabled={locked} renameDisabled={locked || running}
              onSelect={path => void select(project, path)} onRename={beginRename}/>
          </details>
        </>}
        {!projects.length && <p className="corbi-note">No projects in src/projects.</p>}
      </div>
      <details className="corbi-folder-settings"><summary>Data folder</summary><button title={root} disabled={busy || loadingFolder} onClick={() => void openFolder()}><FolderOpen size={14}/>Open existing folder…</button><span title={root}>{root}</span></details>
      <button className="corbi-history-button" aria-pressed={view === 'history'} disabled={!project || locked}
        onClick={() => setView('history')}><Clock3 size={16}/>Run history<ChevronRight size={14}/></button>
    </aside>
    <div className="corbi-main">
      <header className="corbi-context"><span><Layers3 size={16}/> {projectName || 'CoRBi'} <ChevronRight size={14}/> <strong>{view === 'history' ? 'Run history' : job ? 'Job' : 'Script'}</strong></span>
        <span className="corbi-destination" aria-label="Run destination">{env ? targetLabel(connection) : 'Choose a Workbench connection'}</span>
      </header>
      {error && <p className="corbi-error" role="alert">{error}</p>}
      {statusError && <p className="corbi-error" role="alert">{statusError} <button onClick={() => setPollRevision(n => n + 1)}>Retry status</button></p>}
      {view === 'history' && project ? <CorbiRunHistory key={projectName + historyRevision} root={root} project={projectName} run={run} active={active} initial={historyEntry}/>
        : file ? <section className="corbi-editor">
          {selectedJob && project && <nav className="corbi-job-tabs" aria-label="Job editors">
            <button aria-pressed={file === selectedJob} disabled={locked} onClick={() => void select(project, selectedJob)}>Options</button>
            {collector && <button aria-pressed={file === collector} disabled={locked} onClick={() => void select(project, collector)}>Collect URIs</button>}
            {processor && <button aria-pressed={file === processor} disabled={locked} onClick={() => void select(project, processor)}>Process documents</button>}
          </nav>}
          <div className="corbi-panel-head"><Braces size={16}/><strong>{file}</strong>
            {dirty && <span className="corbi-dirty">Unsaved</span>}
            <button disabled={!dirty || locked} onClick={() => void save()}>{saved && !dirty ? <Check size={14}/> : <Save size={14}/>} {saved && !dirty ? 'Saved' : 'Save'}</button></div>
          <div className="editor-body" onKeyDown={e => { if ((e.ctrlKey || e.metaKey) && e.key === 's') { e.preventDefault(); void save(); } }}>
            {original !== null && <QueryEditor label="CoRBi source" source={text} language={file.endsWith('.xqy') ? 'xquery' : /\.(sjs|js)$/.test(file) ? 'javascript' : 'text'}
              disabled={locked} onChange={value => { setText(value); setSaved(false); setDraft(value === original ? null : { project: projectName, file, source: value, baseline: original }); }}/>}</div>
        </section> : <div className="corbi-empty-project"><Layers3 size={26}/><strong>{projectName || 'Your projects'}</strong><p>Create a job to choose your URIs and write a processor.</p><button className="run-button" disabled={locked || running} onClick={() => { setCreating(true); setNewProject(projectName || 'My jobs'); }}><Plus size={14}/>New job</button></div>}
      {running && run ? <div className="corbi-controls"><LoaderCircle className="spin" size={16}/><strong>{run.wet ? 'Wet' : 'Dry'} run in progress</strong><span>{run.job} · {targetLabel(run.connection)}</span>
        <button disabled={locked} onClick={() => void stop()}><Square size={13}/>Stop run</button></div>
        : job && <div className="corbi-controls">
          <label>Dry-run URI limit<input aria-label="Dry-run URI limit" type="number" min="1" step="1" value={limit} placeholder="Default" onChange={e => setLimit(e.target.value)}/></label>
          <span className="corbi-run-job" title={selectedJob}>{selectedJob}</span>
          <div className="corbi-actions">{promotable && <button className="corbi-wet" disabled={!canRun} onClick={() => void start(true)}><Play size={13}/>Execute wet run</button>}
            <button className="run-button" disabled={!canRun || !limitValid} onClick={() => void start(false)}>{starting ? <LoaderCircle className="spin" size={14}/> : <Play size={14}/>} {starting ? 'Preparing CoRB…' : 'Dry run'}</button></div>
        </div>}
      <footer className="corbi-footer" role="status"><span>{!enabled ? `Job disabled in ${env}` : starting ? 'Preparing CoRB · downloading runtime on first use' : dirty ? 'Save changes before running' : run ? `${run.wet ? 'Wet' : 'Dry'} run ${run.status} · ${run.job} · ${targetLabel(run.connection)}` : 'Ready'}</span><span title={root}>{root}</span></footer>
    </div>
  </div>;
}
