import { useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { ArrowRight, Copy, Database, Layers3, Network, RefreshCw, Search, Server, Trees } from 'lucide-react';
import { errorMessage, type Connection } from './api';
import { topologyApi, type TopologySnapshot } from './topology-api';
import { databaseLinks, groupForestsByDatabaseType, hasLink, indexTopology, naturalCompare, projectTopology, type ForestTypeRow, type TopologyIndex, type TopologyProjection, type TopologyScope } from './topology';
import './topology.css';

export function TopologyWorkspace({ active, connection }: { active: boolean; connection: Connection }) {
  const [loaded, setLoaded] = useState<{ key: string; snapshot: TopologySnapshot } | null>(null);
  const [scope, setScope] = useState<TopologyScope>({ kind: 'all' });
  const [view, setView] = useState<'applications' | 'hosts'>('applications');
  const [filter, setFilter] = useState('');
  const [family, setFamily] = useState<string | null>(null);
  const [legendFilter, setLegendFilter] = useState('');
  const [request, setRequest] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [copied, setCopied] = useState(false);
  const currentKey = JSON.stringify([connection.server, connection.appServer, connection.database]);
  const latestKey = useRef(currentKey); latestKey.current = currentKey;
  const snapshot = loaded?.key === currentKey ? loaded.snapshot : null;
  useEffect(() => {
    if (!active || !connection.server || (snapshot && request === 0)) return;
    let current = true;
    setBusy(true); setError('');
    void topologyApi.snapshot(connection).then(next => {
      if (!current || latestKey.current !== currentKey) return;
      setLoaded({ key: currentKey, snapshot: next }); setRequest(0); setCopied(false);
    }).catch(reason => { if (current) setError(errorMessage(reason)); })
      .finally(() => { if (current) setBusy(false); });
    return () => { current = false; };
  }, [active, currentKey, request]);
  const index = useMemo(() => snapshot ? indexTopology(snapshot) : null, [snapshot]);
  const effectiveScope = scope.kind === 'all' || (scope.kind === 'database' && index?.databases.has(scope.id)) ||
    (scope.kind === 'application' && index?.applications.has(scope.id)) ||
    (scope.kind === 'server' && snapshot?.appServers.some(app => app.id === scope.id)) ? scope : { kind: 'all' } as const;
  const projection = useMemo(() => snapshot && index ? projectTopology(snapshot, index, effectiveScope) : null, [snapshot, index, effectiveScope.kind, 'id' in effectiveScope ? effectiveScope.id : '']);
  const forestRows = useMemo(() => projection && index ? groupForestsByDatabaseType(projection.forests, index) : [], [projection, index]);
  const snapshotJson = useMemo(() => snapshot ? JSON.stringify(snapshot, null, 2) : '', [snapshot]);
  const query = filter.trim().toLowerCase();
  const choose = (next: TopologyScope) => { setScope(next); setFamily(null); };
  const title = effectiveScope.kind === 'all' ? 'Cluster overview' : effectiveScope.kind === 'application' ? effectiveScope.id :
    effectiveScope.kind === 'database' ? index?.databases.get(effectiveScope.id)?.name : snapshot?.appServers.find(app => app.id === effectiveScope.id)?.name;
  async function copySnapshot() {
    if (!snapshot) return;
    try { await navigator.clipboard.writeText(snapshotJson); setCopied(true); }
    catch (reason) { setError(`Could not copy snapshot: ${errorMessage(reason)}`); }
  }
  return <section className="topology-workspace" hidden={!active} aria-label="Topology">
    <aside className="topology-sidebar" aria-label="Topology explorer">
      <div className="topology-sidebar-title"><Network size={17}/><strong>TOPOLOGY</strong></div>
      <label className="topology-search"><Search size={14}/><input aria-label="Filter topology explorer" placeholder="Find app server, port, database…" value={filter} onChange={event => setFilter(event.target.value)}/></label>
      <button className={effectiveScope.kind === 'all' ? 'selected' : ''} onClick={() => choose({ kind: 'all' })}><Layers3 size={15}/> Entire cluster</button>
      <p className="topology-section-label">APP SERVERS <span>{snapshot?.appServers.length ?? 0}</span></p>
      {index && [...index.applications].sort(([a], [b]) => naturalCompare(a, b)).map(([name, apps]) => {
        const visible = apps.filter(app => `${name} ${app.name} ${app.port} ${index.groups.get(app.groupId)?.name ?? ''}`.toLowerCase().includes(query));
        return visible.length ? <div className="topology-app-group" key={name}>
          <button className={`topology-group-button ${effectiveScope.kind === 'application' && effectiveScope.id === name ? 'selected' : ''}`} onClick={() => choose({ kind: 'application', id: name })}><Network size={14}/><strong>{name}</strong><span>{apps.length}</span></button>
          {visible.map(app => <button key={app.id} className={`topology-app-button ${effectiveScope.kind === 'server' && effectiveScope.id === app.id ? 'selected' : ''}`} onClick={() => choose({ kind: 'server', id: app.id })} title={`${index.groups.get(app.groupId)?.name} · ${app.kind}`}><span>{app.name}{!app.enabled ? ' (disabled)' : ''}</span><code>:{app.port}</code></button>)}
        </div> : null;
      })}
      <p className="topology-section-label">DATABASES <span>{snapshot?.databases.length ?? 0}</span></p>
      {snapshot?.databases.filter(db => db.name.toLowerCase().includes(query)).sort((a, b) => naturalCompare(a.name, b.name)).map(db => <button className={effectiveScope.kind === 'database' && effectiveScope.id === db.id ? 'selected' : ''} key={db.id} onClick={() => choose({ kind: 'database', id: db.id })}><Database size={14}/><span>{db.name}</span></button>)}
      <p className="topology-sidebar-note">App servers are grouped by their first name token. All connections and replica pairs come from configuration IDs.</p>
    </aside>
    <main className="topology-main">
      <header className="topology-toolbar"><div><h1>{title}</h1><p>{snapshot ? `Configuration snapshot · ${new Date(snapshot.capturedAt).toLocaleString()}` : 'Read the cluster configuration to explore its connections.'}</p></div><button disabled={busy || !connection.server} onClick={() => { setRequest(n => n + 1); setError(''); }}><RefreshCw size={15} className={busy ? 'spin' : ''}/>{busy ? 'Reading configuration…' : 'Refresh'}</button></header>
      {error && <div className="topology-error" role="alert">{error}{snapshot && <p>The previous snapshot is still displayed.</p>}</div>}
      {!connection.server ? <p className="topology-empty">Choose an environment to read its topology.</p> : !snapshot ? <p className="topology-empty" role="status">{busy ? 'Reading app servers, databases, forests, replicas, groups and hosts…' : 'No configuration loaded. Refresh to try again.'}</p> : index && projection && <>
        <div className="topology-summary"><span><Network size={15}/>{projection.servers.length} app servers</span><span><Database size={15}/>{projection.databases.length} databases</span><span><Trees size={15}/>{projection.forests.length} forests</span><span><Server size={15}/>{projection.hosts.length} hosts</span><span className="topology-config-note">Configured placement · local cluster</span></div>
        <div className="topology-view-tabs" role="group" aria-label="Topology layout"><button aria-pressed={view === 'applications'} onClick={() => setView('applications')}>Application map</button><button aria-pressed={view === 'hosts'} onClick={() => setView('hosts')}>Hosts & ports</button>{family && <button onClick={() => setFamily(null)}>Clear forest highlight</button>}</div>
        {projection.warnings.length > 0 && <div className="topology-warning" role="status">{projection.warnings.map(warning => <p key={warning}>{warning}</p>)}</div>}
        {view === 'applications' ? <>
          <section aria-label="App server database connections" className="topology-connections">
            <h2>App servers <ArrowRight size={15}/> databases</h2>
            {!projection.servers.length && <p className="topology-muted">No app server is configured with this database as content or modules.</p>}
            {projection.servers.map(app => <article className="topology-connection-row" key={app.id}>
              <div className="topology-server-node"><strong><Network size={15}/>{app.name}</strong><div><code>:{app.port}</code> · {app.kind.toUpperCase()}{app.secure ? ' · TLS' : ''}{!app.enabled ? ' · Disabled' : ''}</div><small>Group: {index.groups.get(app.groupId)?.name ?? app.groupId}</small><small>Hosts: {app.hostIds.map(id => index.hosts.get(id)?.name ?? id).join(', ') || 'None'}</small><details><summary>Server settings</summary><p>Root: {app.root || '/'}</p><p>Authentication: {app.authentication}</p></details></div>
              <ArrowRight className="topology-connector" size={22}/>
              <div className="topology-database-targets">{[{ role: 'Content', id: app.contentDatabaseId }, { role: 'Modules', id: app.modulesDatabaseId }].map(link => <div key={link.role}><span>{link.role}</span>{hasLink(link.id) ? <button onClick={() => choose({ kind: 'database', id: link.id })}><Database size={15}/>{index.databases.get(link.id)?.name ?? `Missing database ${link.id}`}</button> : <em>{link.role === 'Modules' ? (app.kind.toLowerCase() === 'webdav' ? 'Not applicable' : 'Filesystem') : 'None'}</em>}</div>)}</div>
            </article>)}
          </section>
          <section className="topology-databases" aria-label="Database forest maps"><h2>Database placement</h2>{!projection.databases.length && <p className="topology-muted">No databases are linked to this selection.</p>}{projection.databases.map(db => {
            const members = new Set(db.forestIds);
            for (const id of db.forestIds) index.forests.get(id)?.replicaIds.forEach(replica => members.add(replica));
            const forests = projection.forests.filter(forest => members.has(forest.id));
            const hostIds = new Set(forests.flatMap(forest => [forest.hostId, ...forest.failoverHostIds]));
            return <article className="topology-database-card" key={db.id}>
              <header><Database size={17}/><h3>{db.name}</h3><span>{[...(index.databaseTypes.get(db.id) ?? [])].join(' · ')}</span><small>{db.forestIds.length} attached forests</small></header>
              <div className="topology-db-links">{databaseLinks(db).map(link => <button key={link.role} onClick={() => choose({ kind: 'database', id: link.id })}><span>{link.role}</span><ArrowRight size={12}/>{index.databases.get(link.id)?.name ?? `Missing database ${link.id}`}</button>)}</div>
              {forests.length ? <div className="topology-host-boxes">{projection.hosts.filter(host => hostIds.has(host.id)).map(host => <div className="topology-host-box" key={host.id}><strong><Server size={14}/>{host.name}</strong><small>{index.groups.get(host.groupId)?.name ?? host.groupId}</small><div className="topology-chips">{forests.filter(forest => forest.hostId === host.id).map(forest => <ForestChip key={forest.id} id={forest.id} index={index} family={family} onSelect={setFamily}/>)}{forests.filter(forest => forest.failoverHostIds.includes(host.id)).map(forest => <ForestChip key={`failover:${forest.id}`} id={forest.id} index={index} family={family} onSelect={setFamily} failover/>)}</div></div>)}</div> : <p className="topology-muted">No forests configured.</p>}
            </article>;
          })}</section>
        </> : <HostLayout projection={projection} forestRows={forestRows} index={index} family={family} onSelect={setFamily}/>}
        <section className="topology-legend" aria-label="Forest legend"><header><h2>Forest legend</h2><label className="topology-search"><Search size={14}/><input aria-label="Find forest in legend" placeholder="Find forest or host…" value={legendFilter} onChange={event => setLegendFilter(event.target.value)}/></label></header><p>Solid = configured primary · dashed = replica · S = shared-disk failover host. Matching numbers identify a configured pair. Select a symbol to highlight its family. Databases used for several roles appear under each type.</p>
          <ForestLegend rows={forestRows} index={index} family={family} onSelect={setFamily} filter={legendFilter}/>
        </section>
        <details className="topology-json"><summary>Configuration snapshot JSON</summary><button onClick={() => void copySnapshot()}><Copy size={14}/>{copied ? 'Copied' : 'Copy snapshot'}</button><pre>{snapshotJson}</pre></details>
      </>}
    </main>
  </section>;
}

function ForestChip({ id, index, family, onSelect, failover = false }: { id: string; index: TopologyIndex; family: string | null; onSelect: (id: string | null) => void; failover?: boolean }) {
  const forest = index.forests.get(id), symbol = index.symbols.get(id);
  if (!symbol) return <span className="topology-muted">Missing forest {id}</span>;
  const title = `${forest?.name ?? id} · ${symbol.replica ? 'Replica' : 'Configured primary'}${failover ? ' · Shared-disk failover' : ''}`;
  return <button className={`topology-forest-chip ${symbol.replica ? 'replica' : 'primary'} ${family === symbol.family ? 'highlighted' : ''} ${family && family !== symbol.family ? 'dimmed' : ''}`} style={{ '--forest-hue': symbol.hue } as CSSProperties} title={title} aria-label={`${symbol.label}: ${title}`} aria-pressed={family === symbol.family} onClick={() => onSelect(family === symbol.family ? null : symbol.family)}><Trees size={13}/>{symbol.label}{failover ? ' S' : ''}</button>;
}

function ForestLegend({ rows, index, family, onSelect, filter }: { rows: ForestTypeRow[]; index: TopologyIndex; family: string | null; onSelect: (id: string | null) => void; filter: string }) {
  const query = filter.trim().toLowerCase();
  const visibleRows = rows.map(row => ({ ...row, forests: row.forests.filter(forest =>
    `${row.type} ${forest.name} ${index.hosts.get(forest.hostId)?.name ?? ''} ${index.symbols.get(forest.id)?.label ?? ''}`.toLowerCase().includes(query),
  ) })).filter(row => row.forests.length > 0);
  return <div className="topology-legend-table"><table>
    <thead><tr><th>Symbol</th><th>Forest</th><th>Database</th><th>Host / group</th><th>Relationship</th></tr></thead>
    {visibleRows.map(row => <tbody key={row.type} aria-label={`${row.type} forests`}>
      <tr className="topology-legend-type"><th scope="rowgroup" colSpan={5}>{row.type}</th></tr>
      {row.forests.map(forest => <tr key={forest.id} className={family && index.symbols.get(forest.id)?.family === family ? 'highlighted' : ''}>
        <td><ForestChip id={forest.id} index={index} family={family} onSelect={onSelect}/></td>
        <td><strong>{forest.name}</strong><small>{forest.enabled ? 'Enabled' : 'Disabled'} · {forest.dataDirectory || 'Default data directory'}</small></td>
        <td>{[...(index.forestDatabaseIds.get(forest.id) ?? [])].filter(id => index.databaseTypes.get(id)?.has(row.type)).map(id => <div key={id}>{index.databases.get(id)?.name ?? id}</div>)}{!index.forestDatabaseIds.has(forest.id) && 'Unattached'}</td>
        <td>{index.hosts.get(forest.hostId)?.name ?? forest.hostId}<small>{index.groups.get(index.hosts.get(forest.hostId)?.groupId ?? '')?.name}</small></td>
        <td>{index.masters.has(forest.id) ? `Replica of ${index.masters.get(forest.id)!.map(id => index.forests.get(id)?.name ?? id).join(', ')}` : forest.replicaIds.length ? `Primary → ${forest.replicaIds.map(id => index.forests.get(id)?.name ?? id).join(', ')}` : 'Primary · no replicas'}{forest.failoverHostIds.length > 0 && <small>Shared-disk failover: {forest.failoverHostIds.map(id => index.hosts.get(id)?.name ?? id).join(', ')}</small>}</td>
      </tr>)}
    </tbody>)}
  </table>{!visibleRows.length && <p className="topology-muted">No forests match this selection.</p>}</div>;
}

function HostLayout({ projection, forestRows, index, family, onSelect }: { projection: TopologyProjection; forestRows: ForestTypeRow[]; index: TopologyIndex; family: string | null; onSelect: (id: string | null) => void }) {
  const groups = [...new Set(projection.hosts.map(host => host.groupId))].sort((a, b) => naturalCompare(index.groups.get(a)?.name ?? a, index.groups.get(b)?.name ?? b));
  return <section className="topology-host-layout" aria-label="Hosts and ports">{groups.map(groupId => <section key={groupId}>
    <h2>Group · {index.groups.get(groupId)?.name ?? groupId}</h2>
    <div className="topology-host-columns">{projection.hosts.filter(host => host.groupId === groupId).map(host => <article className="topology-host-column" key={host.id} aria-label={host.name}>
      <h3><Server size={16}/>{host.name}</h3><h4>APP SERVER PORTS</h4>
      {projection.servers.filter(app => app.hostIds.includes(host.id)).map(app => <div className="topology-port-row" key={app.id}>
        <code>:{app.port}</code><div><strong>{app.name}</strong><small>{app.kind.toUpperCase()}{app.secure ? ' · TLS' : ''}{!app.enabled ? ' · Disabled' : ''}</small>
          <small>Content → {index.databases.get(app.contentDatabaseId)?.name ?? (hasLink(app.contentDatabaseId) ? app.contentDatabaseId : 'None')}</small>
          <small>Modules → {index.databases.get(app.modulesDatabaseId)?.name ?? (hasLink(app.modulesDatabaseId) ? app.modulesDatabaseId : app.kind.toLowerCase() === 'webdav' ? 'Not applicable' : 'Filesystem')}</small>
        </div>
      </div>)}
      {!projection.servers.some(app => app.hostIds.includes(host.id)) && <p className="topology-muted">No selected app servers</p>}
      <h4>FORESTS</h4>
      <div className="topology-forest-types">{forestRows.map(row => {
        const local = row.forests.filter(forest => forest.hostId === host.id);
        const failovers = row.forests.filter(forest => forest.failoverHostIds.includes(host.id));
        return <section className="topology-forest-type-row" key={row.type} aria-label={`${row.type} forests`}>
          <h5>{row.type}</h5><div className="topology-chips">
            {local.map(forest => <ForestChip key={forest.id} id={forest.id} index={index} family={family} onSelect={onSelect}/>)}
            {failovers.map(forest => <ForestChip key={`failover:${forest.id}`} id={forest.id} index={index} family={family} onSelect={onSelect} failover/>)}
            {!local.length && !failovers.length && <span className="topology-no-forests">—</span>}
          </div>
        </section>;
      })}</div>
    </article>)}</div>
  </section>)}</section>;
}
