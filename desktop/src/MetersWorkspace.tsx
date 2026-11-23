import { useState, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Copy, RefreshCw } from 'lucide-react';
import { CaptureStatus } from './CaptureStatus';
import { errorMessage, type Connection } from './api';
import { useHostCapture, type Capture } from './host-capture';
import { useSessionState } from './workbench-sessions';
import './meters.css';

interface RawSample { uri: string; xml: string }
const readMeters = (connection: Connection, cached: boolean, database: string) =>
  invoke<Capture<RawSample[]> | null>('raw_meters', { connection, database, cached });
const readDump = (connection: Connection, cached: boolean) =>
  invoke<Capture<string> | null>('support_dump', { connection, cached });

export function MetersWorkspace({ active, connection }: { active: boolean; connection: Connection }) {
  const [view, setView] = useState<'meters' | 'dump'>('meters');
  const [savedDatabase, setDatabase] = useSessionState(`meters.database:${connection.server}`, 'Meters');
  const database = typeof savedDatabase === 'string' && savedDatabase.trim() ? savedDatabase : 'Meters';
  const [draft, setDraft] = useState(database);
  const [sampleUri, setSampleUri] = useState('');
  const meters = useHostCapture(active && view === 'meters', connection, readMeters, database);
  const dump = useHostCapture(active && view === 'dump', connection, readDump);
  const current = view === 'meters' ? meters : dump;
  const samples = meters.capture?.data ?? [];
  const sample = samples.find(item => item.uri === sampleUri) ?? samples[0];
  return <main className="meters-workspace" hidden={!active} aria-label="Meters">
    <header className="topology-toolbar"><div><h1>Meters</h1>{current.capture && <p>Captured {new Date(current.capture.capturedAt).toLocaleString()}</p>}</div>
      <button onClick={current.refresh} disabled={current.busy || !connection.server}><RefreshCw size={15} className={current.busy ? 'spin' : ''}/>{current.busy ? 'Reading MarkLogic…' : 'Refresh'}</button>
    </header>
    <div className="topology-view-tabs" role="group" aria-label="Capture type">
      <button aria-pressed={view === 'meters'} onClick={() => setView('meters')}>Raw meters</button>
      <button aria-pressed={view === 'dump'} onClick={() => setView('dump')}>System status dump</button>
    </div>
    {connection.server ? <>
      {view === 'meters' && <form className="meters-database" onSubmit={event => { event.preventDefault(); setDatabase(draft.trim()); }}>
        <label>Meters database <input value={draft} onChange={event => setDraft(event.target.value)}/></label><button disabled={!draft.trim() || draft.trim() === database}>Use database</button>
      </form>}
      <CaptureStatus {...current}/>
      {view === 'meters' ? sample && <NativeCapture key={sample.uri} content={sample.xml}>
        <label>Raw XML sample <select value={sample.uri} onChange={event => setSampleUri(event.target.value)}>{samples.map(item => <option key={item.uri}>{item.uri}</option>)}</select></label>
      </NativeCapture> : dump.capture && <NativeCapture key={dump.capture.capturedAt} content={dump.capture.data} />}
    </> : <p>Choose an environment to read its meters and system status.</p>}
  </main>;
}

function NativeCapture({ content, children }: { content: string; children?: ReactNode }) {
  const [notice, setNotice] = useState('');
  async function copy() {
    try { await navigator.clipboard.writeText(content); setNotice('Copied'); }
    catch (error) { setNotice(`Could not copy: ${errorMessage(error)}`); }
  }
  return <section className="meters-capture">
    <div>{children}<button onClick={() => void copy()}><Copy size={14}/> Copy capture</button><span role="status">{notice}</span></div>
    <pre>{content}</pre>
  </section>;
}
