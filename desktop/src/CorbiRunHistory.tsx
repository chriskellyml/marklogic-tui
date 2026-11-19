import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Clock3, FileText, Maximize2, Minimize2, Terminal } from 'lucide-react';
import { corbiApi, type RunEntry, type CorbiRun } from './corbi-api';
import { CorbiReportViewer } from './CorbiReportViewer';
import { CorbiLogViewer } from './CorbiLogViewer';
import { errorMessage } from './api';
import { useSessionState } from './workbench-sessions';

export function formatRunId(id: string) {
  return id.replace(/^(\d{4})(\d\d)(\d\d)(\d\d)(\d\d)(\d\d)(.*)$/, '$1-$2-$3  $4:$5:$6$7');
}

export function CorbiRunHistory({ root, project, run, active, initial }: {
  root: string; project: string; run: CorbiRun | null; active: boolean; initial: RunEntry | null;
}) {
  const [entries, setEntries] = useState<RunEntry[]>([]);
  const [selection, setSelection] = useSessionState<RunEntry | null>(`corbi.history:${JSON.stringify([root, project])}`, initial);
  useLayoutEffect(() => { if (initial) setSelection(initial); }, [initial]);
  const [error, setError] = useState('');
  const [loaded, setLoaded] = useState(false);
  useEffect(() => {
    let current = true;
    if (active) void corbiApi.history(root, project).then(rows => {
      if (current) { setEntries(rows); setLoaded(true); setError(''); }
    }).catch(e => { if (current) setError(errorMessage(e)); });
    return () => { current = false; };
  }, [root, project, active, run?.id, run?.status]);

  const selected = entries.find(r => r.id === selection?.id && r.environment === selection.environment) ?? entries[0];
  const currentRun = run?.project === project && run.id === selected?.id && run.historyEnv === selected?.environment ? run : null;
  return <section className="corbi-history" aria-label="Run history">
    <div className="corbi-history-list">
      <h2><Clock3 size={15}/> Run history</h2>
      {entries.map(entry => <button key={entry.environment + '/' + entry.id} className="corbi-history-row"
        aria-pressed={selected === entry} onClick={() => setSelection(entry)}>
        <strong>{formatRunId(entry.id)}</strong><span>{entry.environment}</span>
      </button>)}
      {loaded && !entries.length && <p className="corbi-note">No runs recorded for this project.</p>}
    </div>
    {error && <p role="alert" className="corbi-error">{error}</p>}
    {selected && <ArtifactViewer key={project + '/' + selected.environment + '/' + selected.id + '/' + Boolean(currentRun?.wet)}
      root={root} project={project} entry={selected} currentRun={currentRun} active={active}/>}
  </section>;
}

function ArtifactViewer({ root, project, entry, currentRun, active }: {
  root: string; project: string; entry: RunEntry; currentRun: CorbiRun | null; active: boolean;
}) {
  const [mode, setMode] = useState<'results' | 'files'>('results');
  const [phase, setPhase] = useState('');
  const [file, setFile] = useState('');
  const [files, setFiles] = useState<string[]>([]);
  const [report, setReport] = useState('');
  const [log, setLog] = useState('');
  const [output, setOutput] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);
  const [ratio, setRatio] = useState(75);
  const [expanded, setExpanded] = useState<'report' | 'log' | null>(null);
  const split = useRef<HTMLDivElement>(null);
  const status = currentRun?.status;
  useEffect(() => {
    if (!active) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    const refresh = async () => {
      try {
        const available = await corbiApi.artifacts(root, project, entry);
        const selectedPhase = phase || (currentRun?.wet || available.some(f => f.startsWith('wet-')) ? 'wet' : 'dry');
        const logFile = available.includes(selectedPhase + '-output.log') ? selectedPhase + '-output.log' : available.find(f => f.startsWith(selectedPhase + '-') && f.endsWith('.log'));
        const reportFile = available.includes(selectedPhase + '-report.txt') ? selectedPhase + '-report.txt' : undefined;
        const selectedFile = available.includes(file) ? file : logFile || available[0];
        const read = (name: string | undefined, tail = true) => name ? corbiApi.output(root, project, entry, name, tail) : Promise.resolve('');
        const [reportText, logText, fileText] = mode === 'results'
          ? await Promise.all([read(reportFile, false), read(logFile), Promise.resolve('')])
          : ['', '', await read(selectedFile)];
        if (live) { setFiles(available); setReport(reportText); setLog(logText); setOutput(fileText); setError(''); setLoading(false); }
      } catch (e) { if (live) { setError(errorMessage(e)); setLoading(false); } }
      if (live && status === 'running') timer = setTimeout(refresh, 1000);
    };
    void refresh();
    return () => { live = false; clearTimeout(timer); };
  }, [root, project, entry, file, phase, mode, currentRun?.wet, status, active]);

  useEffect(() => {
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') setExpanded(null); };
    if (expanded) window.addEventListener('keydown', escape);
    return () => window.removeEventListener('keydown', escape);
  }, [expanded]);

  const selectedPhase = phase || (currentRun?.wet || files.some(f => f.startsWith('wet-')) ? 'wet' : 'dry');
  const selectedFile = files.includes(file) ? file : files.find(f => f === selectedPhase + '-output.log') || files[0] || '';
  const resize = (clientY: number) => {
    const box = split.current?.getBoundingClientRect();
    if (box) setRatio(Math.max(15, Math.min(85, 100 * (clientY - box.top) / box.height)));
  };
  const expandButton = (pane: 'report' | 'log') => <button className="icon-button" aria-label={expanded === pane ? 'Restore split view' : `Enlarge ${pane}`} title={expanded === pane ? 'Restore split view (Esc)' : `Enlarge ${pane}`}
    onClick={() => setExpanded(expanded === pane ? null : pane)}>{expanded === pane ? <Minimize2 size={14}/> : <Maximize2 size={14}/>}</button>;
  return <div className="corbi-artifact">
    <div className="corbi-panel-head"><Terminal size={16}/><strong>{formatRunId(entry.id)}</strong>
      {currentRun && <span className="corbi-status" data-status={status}>{currentRun.wet ? 'Wet' : 'Dry'} · {status}</span>}
      <div className="corbi-output-mode" role="group" aria-label="Run view">
        <button aria-pressed={mode === 'results'} onClick={() => { setMode('results'); setLoading(true); }}>Report &amp; log</button>
        <button aria-pressed={mode === 'files'} onClick={() => { setMode('files'); setLoading(true); }}>Files</button>
      </div>
    </div>
    {error && <p className="corbi-error" role="alert">{error}</p>}
    {mode === 'files' ? <>
      <label className="corbi-file-select"><FileText size={14}/>
        <select aria-label="Run artifact" value={selectedFile} onChange={e => { setFile(e.target.value); setLoading(true); }}>{files.map(f => <option key={f}>{f}</option>)}</select></label>
      <pre tabIndex={0} aria-label="Run output">{loading ? 'Loading output…' : output || 'No output yet.'}</pre>
    </> : <>
      {files.some(f => f.startsWith('wet-')) && <label className="corbi-phase-select">Phase<select aria-label="Run phase" value={selectedPhase} onChange={e => { setPhase(e.target.value); setLoading(true); }}>
        <option value="dry">Dry run</option><option value="wet">Wet run</option></select></label>}
      <div className="corbi-output-split" ref={split} style={{ gridTemplateRows: expanded ? 'minmax(0, 1fr)' : `minmax(0, ${ratio}fr) 6px minmax(0, ${100 - ratio}fr)` }}>
        <section className="corbi-output-pane" hidden={expanded === 'log'} aria-label="Report pane">
          <div className="corbi-panel-head"><strong>Report</strong>{expandButton('report')}</div>
          {loading ? <p className="corbi-note">Loading report…</p> : <CorbiReportViewer content={report} waiting={status === 'running'}/>}
        </section>
        {!expanded && <div className="corbi-output-resizer" role="separator" aria-label="Resize report and log" aria-orientation="horizontal"
          tabIndex={0} aria-valuemin={15} aria-valuemax={85} aria-valuenow={Math.round(ratio)}
          onPointerDown={e => { if (e.button === 0) { e.preventDefault(); e.currentTarget.setPointerCapture(e.pointerId); resize(e.clientY); } }}
          onPointerMove={e => { if (e.currentTarget.hasPointerCapture(e.pointerId)) resize(e.clientY); }}
          onPointerUp={e => { if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId); }}
          onKeyDown={e => { if (['ArrowUp', 'ArrowDown', 'Home'].includes(e.key)) { e.preventDefault(); setRatio(r => e.key === 'Home' ? 75 : Math.max(15, Math.min(85, r + (e.key === 'ArrowUp' ? -5 : 5)))); } }}/>}
        <section className="corbi-output-pane" hidden={expanded === 'report'} aria-label="Log pane">
          <div className="corbi-panel-head"><strong>Log</strong>{expandButton('log')}</div>
          {loading ? <p className="corbi-note">Loading log…</p> : <CorbiLogViewer content={log}/>}
        </section>
      </div>
    </>}
  </div>;
}
