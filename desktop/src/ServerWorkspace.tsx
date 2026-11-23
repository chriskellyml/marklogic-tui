import { useEffect, useState } from 'react';
import { Plus, Save, Trash2 } from 'lucide-react';
import { useSessionState } from './workbench-sessions';
import { api, errorMessage, type AppServer, type ClusterEndpoints, type EndpointOverride, type ServerSettings, type TestEndpoint, type Workspace } from './api';

const blank = ():ServerSettings => ({name:'',host:'',port:8000,secure:false,username:'',authType:'digest',insecure:false,color:null,stripeColorSecondary:null,appServers:[]});
const endpoint = ():AppServer => ({name:'',port:8000,secure:false,group:'Default',content_database:null,modules_database:null,favorite:false});
const clusterServices = [
  { kind: 'manage', label: 'Manage', port: 8002, help: 'Database discovery and support status.' },
  { kind: 'query', label: 'Cluster query', port: 8000, help: 'Enter the App Services port for Logs, Topology and raw Meters. Leave it blank to use the selected app server.' },
  { kind: 'admin', label: 'Admin', port: 8001, help: 'Checks that the Admin service responds.' },
] as const;

export function ServerWorkspace({active, workspace, selectedServer, onSaved, onDirtyChange, onSavingChange}: {active:boolean; workspace:Workspace|null; selectedServer:string; onSaved:(name:string)=>Promise<void>; onDirtyChange:(dirty:boolean)=>void; onSavingChange?:(saving:boolean)=>void}) {
  const [selection, setSelection] = useSessionState<string | null>('server.selection', null);
  const [original,setOriginal]=useState<string|null>(null);
  const [form,setForm]=useState<ServerSettings|null>(null);
  const [baseline,setBaseline]=useState('');
  const [error,setError]=useState('');
  const [message,setMessage]=useState('');
  const [busy,setBusy]=useState(false);
  const [test, setTest] = useState<{ key: string; state: 'running' | 'success' | 'failure'; message: string }>();
  const [initialized,setInitialized]=useState(false);
  const dirty=form!==null && JSON.stringify(form)!==baseline;
  useEffect(()=>{onDirtyChange(dirty);},[dirty,onDirtyChange]);
  useEffect(()=>{if(active && workspace && !initialized){setInitialized(true); const name = selection === '' ? '' : workspace.servers.some(item => item.name === selection) ? selection : selectedServer; if(name) void load(name); else startNew();}},[active,workspace,initialized,selectedServer]);
  function startNew(){setSelection('');const next=blank();setOriginal(null);setForm(next);setBaseline(JSON.stringify(next));setError('');setMessage('');setTest(undefined);}
  async function load(name:string){setBusy(true);setError('');setMessage('');setTest(undefined);try{const next=await api.serverSettings(name);setSelection(name);setOriginal(name);setForm(next);setBaseline(JSON.stringify(next));}catch(e){setError(errorMessage(e));}finally{setBusy(false);}}
  function choose(name:string){if(dirty&&!window.confirm('Discard unsaved server settings?'))return;if(name==='')startNew();else void load(name);}
  function update<K extends keyof ServerSettings>(key:K,value:ServerSettings[K]){setForm(old=>old?{...old,[key]:value}:old);setMessage('');setTest(undefined);}
  function updateEndpoint(index:number, changes:Partial<AppServer>){setForm(old=>old?{...old,appServers:old.appServers.map((item,i)=>i===index?{...item,...changes}:item)}:old);setMessage('');setTest(undefined);}
  function updateCluster(kind: typeof clusterServices[number]['kind'], changes: Partial<EndpointOverride>) {
    update('cluster', { ...form?.cluster, [kind]: { ...form?.cluster?.[kind], ...changes } });
  }
  async function testConnection(target: TestEndpoint, key: string) {
    if (!form || busy) return;
    setBusy(true);
    setTest({ key, state: 'running', message: 'Testing connection…' });
    try {
      await api.testServerConnection(original, form, target);
      setTest({ key, state: 'success', message: 'Connection succeeded.' });
    } catch (reason) {
      setTest({ key, state: 'failure', message: errorMessage(reason) });
    } finally {
      setBusy(false);
    }
  }
  function testControl(target: TestEndpoint, label: string, disabled = false) {
    const key = target.kind === 'app' ? `app:${target.index}` : target.kind;
    const result = test?.key === key ? test : undefined;
    return <div className="server-connection-test">
      <button type="button" disabled={disabled || busy || !form?.host.trim()} aria-label={`Test ${label} connection`} onClick={() => void testConnection(target, key)}>Test connection</button>
      {result && <div className={result.state === 'failure' ? 'server-test-error' : undefined} role={result.state === 'failure' ? 'alert' : 'status'}>{result.message}</div>}
    </div>;
  }
  async function save(){if(!form)return;onSavingChange?.(true);setBusy(true);setError('');setMessage('');try{await api.saveServerSettings(original,form);const saved={...form};delete saved.password;setSelection(saved.name);setOriginal(saved.name);setForm(saved);setBaseline(JSON.stringify(saved));await onSaved(saved.name);setMessage('Server settings saved.');}catch(e){setError(errorMessage(e));}finally{setBusy(false);onSavingChange?.(false);}}
  return <section className="server-workspace" aria-label="Server settings" hidden={!active}>
    <aside className="server-list"><div className="server-list-title"><strong>SERVERS</strong><button aria-label="Add server" title="Add server" disabled={busy} onClick={()=>choose('')}><Plus size={16}/></button></div>
      {workspace?.servers.map(s=><button className={original===s.name?'selected':''} key={s.name} disabled={busy} onClick={()=>choose(s.name)}>{s.name}</button>)}
      {original===null && form && <span className="server-new">New server</span>}
    </aside>
    <main className="server-editor">{error && <div role="alert" className="server-error">{error}</div>}{message && <div role="status" className="server-message">{message}</div>}
      {form ? <form onSubmit={e=>{e.preventDefault();void save();}}>
        <div className="server-editor-toolbar"><strong>{original??'New server'}</strong><span>{dirty?'Unsaved changes':''}</span><button className="server-save" type="submit" disabled={busy || !dirty}><Save size={15}/> Save</button></div>
        <fieldset className="server-settings-fields" disabled={busy} aria-label="Profile and endpoints">
        <div className="server-form-grid">
          <label>Name<input required disabled={original!==null} value={form.name} onChange={e=>update('name',e.target.value)}/></label>
          <label>Stripe color 1<input type="color" value={form.color??'#ffcc00'} onChange={e=>update('color',e.target.value)}/></label>
          <label>Stripe color 2<input type="color" value={form.stripeColorSecondary??'#101418'} onChange={e=>update('stripeColorSecondary',e.target.value)}/></label>
          <label>Host<input required value={form.host} placeholder="localhost" onChange={e=>update('host',e.target.value)}/></label>
          <label>Default port<input type="number" min="1" max="65535" required value={form.port||''} onChange={e=>update('port',Number(e.target.value))}/></label>
          <label>Username<input value={form.username} autoComplete="off" onChange={e=>update('username',e.target.value)}/></label>
          <label>Password<input type="password" autoComplete="new-password" placeholder={original?'Leave blank to keep current password':'Password'} value={form.password??''} onChange={e=>update('password',e.target.value||undefined)}/></label>
          <label>Authentication<select value={form.authType} onChange={e=>update('authType',e.target.value as ServerSettings['authType'])}><option value="digest">Digest</option><option value="basic">Basic</option><option value="digestbasic">Digest / Basic</option><option value="application-level">Application level</option></select></label>
          <label className="server-check"><input type="checkbox" checked={form.secure} onChange={e=>update('secure',e.target.checked)}/> Use HTTPS by default</label>
          <label className="server-check"><input type="checkbox" checked={form.insecure} onChange={e=>update('insecure',e.target.checked)}/> Allow invalid TLS certificates</label>
        </div>
        {testControl({ kind: 'default' }, 'default server')}
        <p className="server-help">The selected environment has a striped marker below the top bar. Unset stripe colors default to yellow and the Workbench background. Profiles are saved in {workspace?.configPath}. Passwords are stored in that local configuration file.</p>
        <div className="server-section-title"><strong>CLUSTER ENDPOINTS</strong></div>
        <p className="server-help">Enter the local ports when using port forwarding. Tests use the settings above, including any unsaved password.</p>
        <div className="server-form-grid">
          <label>Cluster authentication
            <select value={form.cluster?.auth_type ?? ''} onChange={event => update('cluster', {
              ...form.cluster, auth_type: (event.target.value || undefined) as ClusterEndpoints['auth_type'],
            })}>
              <option value="">Use profile authentication</option>
              <option value="digestbasic">Digest / Basic</option>
              <option value="digest">Digest</option>
              <option value="basic">Basic</option>
              <option value="application-level">Application level</option>
            </select>
          </label>
        </div>
        {clusterServices.map(({ kind, label, port, help }) => {
          const settings = form.cluster?.[kind];
          const usesAppServer = kind === 'query' && !settings?.port;
          return <div className="server-endpoint" key={kind}>
            <strong>{label}</strong>
            <p className="server-help">{help}</p>
            <div className="server-form-grid">
              <label>{label} port
                <input type="number" min="1" max="65535" value={settings?.port ?? ''}
                  placeholder={String(port)} onChange={event => updateCluster(kind, { port: event.target.value === '' ? undefined : event.target.valueAsNumber })} />
              </label>
              <label>{label} protocol
                <select disabled={usesAppServer} value={settings?.secure === undefined ? '' : String(settings.secure)}
                  onChange={event => updateCluster(kind, { secure: event.target.value === '' ? undefined : event.target.value === 'true' })}>
                  <option value="">Use profile protocol</option>
                  <option value="false">HTTP</option>
                  <option value="true">HTTPS</option>
                </select>
              </label>
            </div>
            {testControl({ kind }, label, usesAppServer)}
          </div>;
        })}
        <div className="server-section-title"><strong>APP SERVERS</strong><button type="button" onClick={()=>update('appServers',[...form.appServers,endpoint()])}><Plus size={14}/> Add app server</button></div>
        {form.appServers.map((item,i)=><div className="server-endpoint" key={i}>
          <div className="server-endpoint-head"><strong>App server {i+1}</strong><button type="button" aria-label={`Remove app server ${i+1}`} title="Remove app server" onClick={()=>update('appServers',form.appServers.filter((_,n)=>n!==i))}><Trash2 size={15}/></button></div>
          <div className="server-form-grid">
            <label>App server name<input required value={item.name} onChange={e=>updateEndpoint(i,{name:e.target.value})}/></label>
            <label>Port<input type="number" min="1" max="65535" required value={item.port||''} onChange={e=>updateEndpoint(i,{port:Number(e.target.value)})}/></label>
            <label>Group<input value={item.group??'Default'} onChange={e=>updateEndpoint(i,{group:e.target.value})}/></label>
            <label>Content database<input value={item.content_database??''} onChange={e=>updateEndpoint(i,{content_database:e.target.value||null})}/></label>
            <label>Modules database<input value={item.modules_database??''} onChange={e=>updateEndpoint(i,{modules_database:e.target.value||null})}/></label>
            <label className="server-check"><input type="checkbox" checked={item.secure} onChange={e=>updateEndpoint(i,{secure:e.target.checked})}/> HTTPS</label>
            <label className="server-check"><input type="checkbox" checked={item.favorite??false} onChange={e=>updateEndpoint(i,{favorite:e.target.checked})}/> Favorite</label>
          </div>
          {testControl({ kind: 'app', index: i }, item.name || `app server ${i + 1}`)}
        </div>)}
        {!form.appServers.length && <p className="server-help">No app servers configured. The default port above will be used.</p>}
        </fieldset>
      </form> : <p className="server-help">{busy?'Loading server settings…':'Select a server or add one.'}</p>}
    </main>
  </section>;
}
