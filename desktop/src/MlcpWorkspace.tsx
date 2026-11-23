import { useEffect, useId, useRef, useState } from 'react';
import { Copy, Download, FolderOpen, LoaderCircle, Pencil, Upload, Plus, Trash2 } from 'lucide-react';
import { errorMessage, type Connection, type ServerProfile } from './api';
import { mlcpApi, type Selection, type Transfer, type TransferResult } from './mlcp-api';
import { restBaseUrl } from './rest-api';
import { useSessionState } from './workbench-sessions';
import './mlcp.css';

type Props = {
  active: boolean;
  connection: Connection;
  servers: ServerProfile[];
  busy: boolean;
  onBusy: (busy: boolean) => void;
};

type Run = {
  direction: Transfer['direction'];
  endpoint: string;
} & ({ status: 'running' } | { status: 'complete'; result: TransferResult } | { status: 'failed'; message: string });

type PullJob = {
  id: string; name: string; source: Connection; folder: string;
  filter: 'all' | 'collections' | 'directories'; filterValue: string; mode: 'timestamp' | 'replace';
};
type Archive = { id: string; name: string; path: string };
type JobData = { archives: { path: string; size: number; files: number }[]; log: string };
function formatSize(size: number | undefined) {
  if (size === undefined) return 'Size unavailable';
  if (size < 1024) return `${size} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let value = size / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}
function defaultName(prefix: string) {
  return `${prefix} ${new Date().toLocaleString()}`;
}

function ConnectionFields({ label, value, servers, onChange }: {
  label: 'Source' | 'Destination';
  value: Connection;
  servers: ServerProfile[];
  onChange: (connection: Connection | null) => void;
}) {
  const server = servers.find(item => item.name === value.server);
  function choose(name: string, appServer: string | null) {
    const profile = servers.find(item => item.name === name);
    const endpoint = profile?.appServers.find(item => item.name === appServer);
    onChange({ server: name, appServer, database: endpoint?.content_database ?? 'Documents' });
  }
  return <div className="mlcp-connection">
    <div className="mlcp-connection-heading">
      <strong>{label} connection</strong>
      <button type="button" onClick={() => onChange(null)}>Use active connection</button>
    </div>
    <div className="mlcp-connection-fields">
      <label>Server<select aria-label={`${label} server`} value={server?.name ?? ''} onChange={event => choose(event.target.value, null)}>
        {!server && <option value="" disabled>Choose a saved server</option>}
        {servers.map(item => <option key={item.name}>{item.name}</option>)}
      </select></label>
      <label>App server<select aria-label={`${label} app server`} value={value.appServer ?? ''} disabled={!server}
        onChange={event => choose(value.server, event.target.value || null)}>
        <option value="">Profile default</option>
        {value.appServer && !server?.appServers.some(item => item.name === value.appServer) && <option value={value.appServer} disabled>{value.appServer} (unavailable)</option>}
        {server?.appServers.map(item => <option key={item.name} value={item.name}>{item.name} :{item.port}</option>)}
      </select></label>
      <label>Database<input aria-label={`${label} database`} value={value.database} disabled={!server}
        onChange={event => onChange({ ...value, database: event.target.value })} /></label>
    </div>
  </div>;
}

function validConnection(servers: ServerProfile[], connection: Connection) {
  const profile = servers.find(item => item.name === connection.server);
  return Boolean(profile && connection.database.trim() &&
    (!connection.appServer || profile.appServers.some(item => item.name === connection.appServer)));
}

function describeConnection(servers: ServerProfile[], connection: Connection) {
  const profile = servers.find(item => item.name === connection.server);
  const address = restBaseUrl(profile, connection.appServer);
  return [connection.server, connection.appServer, connection.database, address].filter(Boolean).join(' / ');
}

export function MlcpWorkspace({ active, connection, servers, busy, onBusy }: Props) {
  const heading = useId();
  const [sourceChoice, setSource] = useSessionState<Connection | null>('mlcp.source', null);
  const [destinationChoice, setDestination] = useSessionState<Connection | null>('mlcp.destination', null);
  const [outputFolder, setOutputFolder] = useSessionState('mlcp.outputFolder', '');
  const [archive, setArchive] = useSessionState('mlcp.archive', '');
  const [filter, setFilter] = useState<'all' | 'collections' | 'directories'>('all');
  const [filterValue, setFilterValue] = useState('');
  const [jobs, setJobs] = useSessionState<PullJob[]>('mlcp.jobs', []);
  const [archives, setArchives] = useSessionState<Archive[]>('mlcp.archives', []);
  const [jobId, setJobId] = useSessionState('mlcp.selectedJob', '');
  const [jobEditing, setJobEditing] = useState(false);
  const [mode, setMode] = useState<'timestamp' | 'replace'>('timestamp');
  const [output, setOutput] = useState('');
  const [jobData, setJobData] = useState<Record<string, JobData>>({});
  const [archiveSizes, setArchiveSizes] = useState<Record<string, number>>({});
  const [archiveFiles, setArchiveFiles] = useState<Record<string, number>>({});
  const [editingJob, setEditingJob] = useState('');
  const [editingArchive, setEditingArchive] = useState('');
  const [copyPermissions, setCopyPermissions] = useState(true);
  const [run, setRun] = useState<Run | null>(null);
  const [pickerError, setPickerError] = useState('');
  const [preview, setPreview] = useState<{ uris: string[]; endpoint: string } | null>(null);
  const [previewBusy, setPreviewBusy] = useState(false);
  const [previewError, setPreviewError] = useState('');
  const [outputTab, setOutputTab] = useState<'log' | 'preview'>('log');
  const previewVersion = useRef(0);
  const source = sourceChoice ?? connection;
  const destination = destinationChoice ?? connection;
  const running = run?.status === 'running';
  const disabled = busy || running;
  const selection: Selection = filter === 'all' ? { kind: 'all' } : { kind: filter, value: filterValue.trim() };

  useEffect(() => {
    previewVersion.current++;
    setPreview(null);
    setPreviewError('');
  }, [source.server, source.appServer, source.database, filter, filterValue]);

  async function previewUris() {
    const version = previewVersion.current;
    const selected = { ...source, database: source.database.trim() };
    setPreviewBusy(true);
    setOutputTab('preview');
    setPreviewError('');
    setPreview(null);
    onBusy(true);
    try {
      const uris = await mlcpApi.previewUris(selected, selection);
      if (version === previewVersion.current) setPreview({ uris, endpoint: describeConnection(servers, selected) });
    } catch (error) {
      if (version === previewVersion.current) setPreviewError(errorMessage(error));
    } finally {
      setPreviewBusy(false);
      onBusy(false);
    }
  }

  useEffect(() => {
    let cancelled = false;
    void Promise.all(jobs.filter(job => job.folder).map(async job => {
      try { return [job.id, await mlcpApi.jobData(job.folder, job.id)] as const; }
      catch { return [job.id, { archives: [], log: '' }] as const; }
    })).then(entries => { if (!cancelled) setJobData(Object.fromEntries(entries)); });
    return () => { cancelled = true; };
  }, [jobs]);

  useEffect(() => {
    let cancelled = false;
    void Promise.all(archives.map(async item => {
      const [size, files] = await Promise.allSettled([mlcpApi.archiveSize(item.path), mlcpApi.archiveFiles(item.path)]);
      return [item.path, size.status === 'fulfilled' ? size.value : undefined, files.status === 'fulfilled' ? files.value : undefined] as const;
    })).then(entries => { if (!cancelled) {
      setArchiveSizes(Object.fromEntries(entries.filter((entry): entry is readonly [string, number, number | undefined] => entry[1] !== undefined).map(([path, size]) => [path, size])));
      setArchiveFiles(Object.fromEntries(entries.filter((entry): entry is readonly [string, number | undefined, number] => entry[2] !== undefined).map(([path, , files]) => [path, files])));
    } });
    return () => { cancelled = true; };
  }, [archives]);

  useEffect(() => {
    if (!run && jobId && jobData[jobId]) setOutput(jobData[jobId].log);
  }, [jobData, jobId, run]);

  useEffect(() => {
    const job = jobs.find(item => item.id === jobId);
    if (job) {
      setSource(job.source); setOutputFolder(job.folder); setFilter(job.filter);
      setFilterValue(job.filterValue); setMode(job.mode);
    }
  // Restore the last selection once when the workspace is mounted.
  }, []);

  async function refreshJob(job: PullJob) {
    const data = await mlcpApi.jobData(job.folder, job.id);
    setJobData(old => ({ ...old, [job.id]: data }));
  }

  async function deleteJob(job: PullJob) {
    try {
      if (!await mlcpApi.confirmDeleteJob(job.name)) return;
      const deleteData = await mlcpApi.confirmDeleteData(job.name, job.folder);
      if (deleteData) await mlcpApi.deleteJobData(job.folder, job.id);
      const removed = new Set((jobData[job.id]?.archives ?? []).map(item => item.path));
      if (deleteData) {
        setArchives(old => old.filter(item => !removed.has(item.path)));
        if (removed.has(archive)) setArchive('');
      }
      setJobs(old => old.filter(item => item.id !== job.id));
      if (jobId === job.id) { setJobId(''); setJobEditing(false); setOutput(''); setRun(null); }
    } catch (error) { setPickerError(errorMessage(error)); }
  }

  async function deleteDownload(item: {id: string; name: string; path: string}) {
    const job = jobs.find(candidate => jobData[candidate.id]?.archives.some(entry => entry.path === item.path));
    if (!job || !await mlcpApi.confirmDeleteDownload(item.name)) return;
    try {
      await mlcpApi.deleteDownload(job.folder, job.id, item.path);
      setArchives(old => old.filter(entry => entry.path !== item.path));
      if (archive === item.path) setArchive('');
      await refreshJob(job);
    } catch (error) { setPickerError(errorMessage(error)); }
  }

  async function pick(choose: () => Promise<string | null>, usePath: (path: string) => void) {
    setPickerError('');
    try {
      const path = await choose();
      if (path) usePath(path);
    } catch (error) {
      setPickerError(errorMessage(error));
    }
  }

  function saveJob(id = jobId || crypto.randomUUID()) {
    const existing = jobs.find(job => job.id === id);
    const job: PullJob = { id, name: existing?.name || defaultName('Pull'), source: { ...source }, folder: outputFolder, filter, filterValue, mode };
    setJobs([job, ...jobs.filter(item => item.id !== id)]);
    setJobId(id);
    setJobEditing(false);
    return id;
  }

  function selectJob(id: string) {
    setJobId(id);
    setJobEditing(false);
    const job = jobs.find(item => item.id === id);
    if (job) {
      setSource(job.source); setOutputFolder(job.folder); setFilter(job.filter);
      setFilterValue(job.filterValue); setMode(job.mode);
      setRun(null); setOutput(jobData[job.id]?.log ?? '');
      setOutputTab('log');
    }
  }

  function duplicateJob(job: PullJob) {
    const copy = { ...job, id: crypto.randomUUID(), name: `${job.name} copy` };
    setJobs(old => [copy, ...old]);
    setJobId(copy.id);
    setSource(copy.source);
    setOutputFolder(copy.folder);
    setFilter(copy.filter);
    setFilterValue(copy.filterValue);
    setMode(copy.mode);
    setJobEditing(true);
    setRun(null);
    setOutput('');
  }

  function rememberArchive(path: string) {
    setArchives(old => old.some(item => item.path === path) ? old :
      [{ id: crypto.randomUUID(), name: defaultName('Archive'), path }, ...old]);
    setArchive(path);
  }

  async function transfer(request: Transfer, selected: Connection) {
    if (disabled || !validConnection(servers, selected)) return;
    const connection = { ...selected, database: selected.database.trim() };
    const endpoint = describeConnection(servers, connection);
    setPickerError('');
    onBusy(true);
    try {
      if (request.direction === 'push' && !await mlcpApi.confirmPush(request.folder, endpoint)) return;
      setOutputTab('log');
      setOutput('');
      setRun({ direction: request.direction, endpoint, status: 'running' });
      const result = await mlcpApi.transfer(connection, request, setOutput);
      if (request.direction === 'pull') {
        rememberArchive(result.archive);
        if (request.jobId) {
          try { await refreshJob({ id: request.jobId, name: '', source: connection, folder: request.folder, filter, filterValue, mode }); }
          catch (error) { setPickerError(errorMessage(error)); }
        }
      }
      setRun({ direction: request.direction, endpoint, status: 'complete', result });
    } catch (error) {
      if (request.direction === 'pull' && request.jobId) {
        try { await refreshJob({ id: request.jobId, name: '', source: connection, folder: request.folder, filter, filterValue, mode }); } catch { /* The error is shown below. */ }
      }
      setRun({ direction: request.direction, endpoint, status: 'failed', message: errorMessage(error) });
    } finally {
      onBusy(false);
    }
  }

  const discovered = Object.values(jobData).flatMap(data => data.archives);
  const downloads = [
    ...archives.map(item => ({ ...item, size: discovered.find(entry => entry.path === item.path)?.size ?? archiveSizes[item.path] })),
    ...discovered.filter(item => !archives.some(saved => saved.path === item.path))
      .map(item => ({ id: item.path, name: item.path.split('/').filter(Boolean).slice(-2).join(' / '), ...item })),
  ];

  if (!active) return null;

  return <main className="mlcp-workspace" aria-labelledby={heading}>
    <aside className="mlcp-sidebar mlcp-jobs" aria-label="Saved MLCP jobs">
      <div className="mlcp-sidebar-title"><strong>MLCP JOBS</strong><button aria-label="New pull job" disabled={disabled} onClick={() => { setJobId(''); setJobEditing(true); setOutput(''); setRun(null); }}><Plus size={16}/></button></div>
      {jobs.map(job => <div key={job.id} className={`mlcp-list-item ${job.id === jobId ? 'selected' : ''}`}>
        {editingJob === job.id ? <input aria-label="Job name" autoFocus value={job.name}
          onChange={event => setJobs(old => old.map(item => item.id === job.id ? { ...item, name: event.target.value } : item))}
          onBlur={() => setEditingJob('')} onKeyDown={event => { if (event.key === 'Enter') event.currentTarget.blur(); }} /> :
          <button className="mlcp-list-select" disabled={disabled} onClick={() => selectJob(job.id)}><strong>{job.name || 'Untitled job'}</strong><small>{jobData[job.id]?.archives.length ?? 0} downloads</small></button>}
        <button disabled={disabled} title="Rename job" aria-label={`Rename job ${job.name}`} onClick={() => { selectJob(job.id); setJobEditing(true); setEditingJob(job.id); }}><Pencil size={14} aria-hidden="true"/></button>
        <button disabled={disabled} title="Duplicate job" aria-label={`Duplicate job ${job.name}`} onClick={() => duplicateJob(job)}><Copy size={14} aria-hidden="true"/></button>
        <button disabled={disabled} title="Delete job" aria-label={`Delete job ${job.name}`} onClick={() => void deleteJob(job)}><Trash2 size={14} aria-hidden="true"/></button>
      </div>)}
      {!jobs.length && <p>No saved jobs yet.</p>}
    </aside>
    <div className="mlcp-content">
      <header className="mlcp-header"><h1 id={heading}>MLCP</h1></header>
      {!servers.length && <p className="mlcp-message">Add a server in Connection to start a transfer.</p>}
      {pickerError && <p role="alert" className="mlcp-message mlcp-error">{pickerError}</p>}
      <div className="mlcp-transfer-grid">
        <fieldset disabled={disabled}>
          <legend><Download size={17} aria-hidden="true" /> Pull</legend>
          <div className="mlcp-connection-heading"><strong>{jobs.find(job => job.id === jobId)?.name || 'New pull job'}</strong>
            {jobId && !jobEditing && <button type="button" disabled={disabled} onClick={() => setJobEditing(true)}>Edit</button>}
            {jobEditing && <button type="button" disabled={!validConnection(servers, source) || !outputFolder} onClick={() => saveJob()}>{jobId ? 'Save changes' : 'Save job'}</button>}
          </div>
          <fieldset className="mlcp-pull-settings" disabled={!jobEditing && Boolean(jobId)}>
          <ConnectionFields label="Source" value={source} servers={servers} onChange={setSource} />
          <label>Documents to pull<select value={filter} onChange={event => {
            setFilter(event.target.value as typeof filter);
            setFilterValue('');
          }}>
            <option value="all">All documents</option>
            <option value="collections">By collection</option>
            <option value="directories">By directory</option>
          </select></label>
          {filter !== 'all' && <label>{filter === 'collections' ? 'Collections' : 'Directories'}
            <input placeholder={filter === 'collections' ? 'orders, customers' : '/orders/, /customers/'} value={filterValue}
              onChange={event => setFilterValue(event.target.value)} />
            <small>Separate entries with commas.</small>
          </label>}
          <div className="mlcp-folder">
            <span>Save to folder</span>
            <div>
              <input aria-label="Export folder" placeholder="Choose a local folder" value={outputFolder} readOnly />
              <button type="button" aria-label="Choose export folder" onClick={() => void pick(() => mlcpApi.chooseFolder('Choose where to save the archive'), setOutputFolder)}>
                <FolderOpen size={15} aria-hidden="true" /> Browse…
              </button>
            </div>
          </div>
          <label>When pulling again<select aria-label="Archive folder behavior" value={mode} onChange={event => setMode(event.target.value as typeof mode)}>
            <option value="timestamp">Create a timestamped folder</option>
            <option value="replace">Replace this job's previous archive</option>
          </select></label>
          </fieldset>
          <div className="mlcp-pull-actions">
          <button type="button" className="mlcp-action" disabled={disabled || !validConnection(servers, source) || !outputFolder || (filter !== 'all' && !filterValue.trim())}
            onClick={() => {
              const id = !jobId || jobEditing ? saveJob() : jobId;
              void transfer({ direction: 'pull', folder: outputFolder,
                selection,
                jobId: id, replace: mode === 'replace' }, source);
            }}>
            <Download size={16} aria-hidden="true" /> Pull archive
          </button>
          <button type="button" className="mlcp-preview-button" disabled={disabled || previewBusy || !validConnection(servers, source) || (filter !== 'all' && !filterValue.trim())}
            onClick={() => void previewUris()}>{previewBusy ? 'Loading…' : 'Preview URIs'}</button>
          </div>
        </fieldset>
        <fieldset disabled={disabled}>
          <legend><Upload size={17} aria-hidden="true" /> Push</legend>
          <ConnectionFields label="Destination" value={destination} servers={servers} onChange={setDestination} />
          <div className="mlcp-folder">
            <span>Archive to push</span>
            <div>
              <input aria-label="MLCP archive folder" placeholder="Choose a folder containing MLCP archive ZIP files" value={archive} readOnly />
              <button type="button" aria-label="Choose archive folder" onClick={() => void pick(() => mlcpApi.chooseFolder('Choose an MLCP archive folder'), rememberArchive)}>
                <FolderOpen size={15} aria-hidden="true" /> Browse…
              </button>
            </div>
          </div>
          <label className="mlcp-checkbox">
            <input type="checkbox" checked={copyPermissions} onChange={event => setCopyPermissions(event.target.checked)} />
            <span>Copy document permissions</span>
          </label>
          <button type="button" className="mlcp-action" disabled={!validConnection(servers, destination) || !archive}
            onClick={() => void transfer({ direction: 'push', folder: archive, copyPermissions }, destination)}>
            <Upload size={16} aria-hidden="true" /> Push archive
          </button>
        </fieldset>
      </div>
      <section className="mlcp-run" aria-label="MLCP output">
        <div className="mlcp-output-tabs" role="tablist" aria-label="MLCP output views">
          <button role="tab" id="mlcp-log-tab" aria-controls="mlcp-log-panel" aria-selected={outputTab === 'log'} onClick={() => setOutputTab('log')}>Log</button>
          <button role="tab" id="mlcp-preview-tab" aria-controls="mlcp-preview-panel" aria-selected={outputTab === 'preview'} onClick={() => setOutputTab('preview')}>URI preview</button>
        </div>
        <div role="tabpanel" id="mlcp-log-panel" aria-labelledby="mlcp-log-tab" hidden={outputTab !== 'log'} className="mlcp-output-panel">
        <div role={run?.status === 'failed' ? 'alert' : 'status'} className={run?.status === 'failed' ? 'mlcp-error' : undefined}>
          <strong>{run?.status === 'running' && <LoaderCircle size={16} className="spin" aria-hidden="true" />}
            {run ? `${run.direction === 'pull' ? 'Pull' : 'Push'} ${run.status === 'running' ? 'in progress' : run.status === 'complete' ? 'complete' : 'failed'}` : 'MLCP output'}
          </strong>
          {run && <p>{run.endpoint}</p>}
          {run?.status === 'failed' && <p>{run.message.split('\n')[0]}</p>}
          {run?.status === 'complete' && <p>{run.result.parts} archive {run.result.parts === 1 ? 'file' : 'files'} · {run.result.archive}</p>}
        </div>
        <pre>{output || (run?.status === 'failed' ? run.message : 'No output yet.')}</pre>
        </div>
        <div role="tabpanel" id="mlcp-preview-panel" aria-labelledby="mlcp-preview-tab" hidden={outputTab !== 'preview'} className="mlcp-output-panel mlcp-preview">
          {previewBusy && <p role="status">Loading URIs…</p>}
          {previewError && <pre role="alert">{previewError}</pre>}
          {preview && <><p>{preview.endpoint} · {preview.uris.length > 50 ? 'First 50 matching URIs' : `${preview.uris.length} matching ${preview.uris.length === 1 ? 'URI' : 'URIs'}`}</p>
            {preview.uris.length ? <ul>{preview.uris.slice(0, 50).map(uri => <li key={uri}>{uri}</li>)}</ul> : <p>No matching documents.</p>}
          </>}
          {!preview && !previewBusy && !previewError && <p>Select Preview URIs to see matching documents.</p>}
        </div>
      </section>
    </div>
    <aside className="mlcp-sidebar mlcp-downloads" aria-label="Downloaded archives">
      <div className="mlcp-sidebar-title"><strong>DOWNLOADS</strong><span>{downloads.length}</span></div>
      {downloads.map(item => <div key={item.id} className={`mlcp-list-item ${item.path === archive ? 'selected' : ''}`}>
        {editingArchive === item.path && archives.some(saved => saved.path === item.path) ?
          <input aria-label="Archive name" autoFocus value={item.name} onChange={event => setArchives(old => old.map(saved => saved.path === item.path ? { ...saved, name: event.target.value } : saved))}
            onBlur={() => setEditingArchive('')} onKeyDown={event => { if (event.key === 'Enter') event.currentTarget.blur(); }}/> :
          <button className="mlcp-list-select" disabled={disabled} title={item.path} onClick={() => setArchive(item.path)}><strong>{item.name || 'Untitled archive'}</strong><small>{jobData[ jobs.find(job => jobData[job.id]?.archives.some(entry => entry.path === item.path))?.id ?? '' ]?.archives.find(entry => entry.path === item.path)?.files ?? archiveFiles[item.path] ?? 0} files · {formatSize(item.size)}</small></button>}
        <button disabled={disabled} title="Rename archive" aria-label={`Rename archive ${item.name}`} onClick={() => {
          if (!archives.some(saved => saved.path === item.path)) setArchives(old => [{ id: crypto.randomUUID(), name: item.name, path: item.path }, ...old]);
          setEditingArchive(item.path);
        }}><Pencil size={14} aria-hidden="true"/></button>
        {jobs.some(job => jobData[job.id]?.archives.some(entry => entry.path === item.path)) &&
          <button disabled={disabled} aria-label={`Delete download ${item.name}`} onClick={() => void deleteDownload(item)}><Trash2 size={13}/></button>}
      </div>)}
      {!downloads.length && <p>Pull a job or browse for an archive to get started.</p>}
    </aside>
  </main>;
}
