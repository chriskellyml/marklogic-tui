import { useEffect, useState } from 'react';
import { ChevronDown, ChevronRight, RefreshCw } from 'lucide-react';
import { errorMessage, type Connection } from './api';
import { openapiApi, parseOpenApi, type ApiOperation, type ParsedSpec } from './openapi';

export function OpenApiExplorer({ active, connection, database, disabled, onOpen }: { active: boolean; connection: Connection; database: string; disabled: boolean; onOpen: (operation: ApiOperation, uri: string) => void }) {
  const [uris, setUris] = useState<string[]>([]);
  const [uri, setUri] = useState('');
  const [spec, setSpec] = useState<ParsedSpec | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const [collapsed, setCollapsed] = useState<string[]>([]);
  const [filter, setFilter] = useState('');
  const [visited, setVisited] = useState(false);
  useEffect(() => { if (active) setVisited(true); }, [active]);
  useEffect(() => {
    if (!visited || !connection.server || !database.trim()) return;
    let alive = true; setLoading(true); setError(''); setSpec(null);
    void openapiApi.discover(connection, database).then(list => { if (alive) { setUris(list); setUri(current => list.includes(current) ? current : list[0] ?? ''); } }).catch(reason => { if (alive) setError(errorMessage(reason)); }).finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [visited, connection.server, connection.appServer, database, revision]);
  useEffect(() => {
    if (!uri) return;
    let alive = true; setLoading(true); setSpec(null); setError('');
    void openapiApi.load(connection, database, uri).then(document => { if (alive) setSpec(parseOpenApi(document)); }).catch(reason => { if (alive) setError(`${uri}: ${errorMessage(reason)}`); }).finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [uri, revision]);
  return <div className="rest-openapi" aria-label="OpenAPI explorer">
    <div className="rest-openapi-heading"><strong>Modules · {database}</strong><button aria-label="Refresh OpenAPI specs" disabled={loading || !connection.server} onClick={() => setRevision(value => value + 1)}><RefreshCw size={14}/></button></div>
    <label className="rest-spec-picker">Specification<select aria-label="OpenAPI specification" value={uri} onChange={event => setUri(event.target.value)} disabled={loading || !uris.length}>{uris.map(path => <option key={path}>{path}</option>)}</select></label>
    <p className="rest-hint">Host and authentication follow the environment. API base paths come from the spec.</p>
    <input className="rest-operation-filter" aria-label="Filter API operations" placeholder="Find an operation…" value={filter} onChange={event => setFilter(event.target.value)}/>
    {loading && <p className="rest-hint" role="status">Loading OpenAPI specs…</p>}
    {error && <div className="rest-error" role="alert">{error}</div>}
    {!loading && !error && !uris.length && <p className="rest-hint">No openapi.yml, openapi.yaml or openapi.json files found. Choose a modules database and refresh.</p>}
    {spec && <><h3>{spec.title}</h3>{[...new Set(spec.operations.map(op => op.group))].map(group => <div key={group}><button className="rest-api-group" aria-expanded={!collapsed.includes(group) || Boolean(filter)} onClick={() => setCollapsed(old => old.includes(group) ? old.filter(item => item !== group) : [...old, group])}>{collapsed.includes(group) && !filter ? <ChevronRight size={13}/> : <ChevronDown size={13}/>} {group}</button>{(!collapsed.includes(group) || filter) && spec.operations.filter(op => op.group === group && `${op.method} ${op.path} ${op.name}`.toLowerCase().includes(filter.toLowerCase())).map(op => <div key={op.id}><button className="rest-request-item" disabled={disabled || !op.draft} title={op.error || op.path} onClick={() => onOpen(op, uri)}><span className={`rest-method method-${op.method.toLowerCase()}`}>{op.method}</span><span><strong>{op.name}</strong><small>{op.path}</small></span></button>{op.error && <p className="rest-hint">{op.error}</p>}</div>)}</div>)}</>}
  </div>;
}
