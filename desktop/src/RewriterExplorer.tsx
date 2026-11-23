import { useMemo } from 'react';
import { useSessionState } from './workbench-sessions';
import { ChevronDown, ChevronRight, RefreshCw } from 'lucide-react';
import { errorMessage, type Connection } from './api';
import { CaptureStatus } from './CaptureStatus';
import { useHostCapture } from './host-capture';
import { parseRewriter, rewriterApi, type RewriterRoute } from './rewriter';

export function RewriterExplorer({ active, connection, disabled, onOpen }: { active: boolean; connection: Connection; disabled: boolean; onOpen: (route: RewriterRoute, path: string) => void }) {
  const stateKey = `rest.rewriter:${JSON.stringify([connection.server, connection.appServer])}`;
  const capture = useHostCapture(active, connection, rewriterApi.load, connection.appServer ?? '');
  const source = capture.capture?.data;
  const { routes, parseError } = useMemo(() => {
    try { return { routes: source?.xml ? parseRewriter(source.xml) : [], parseError: '' }; }
    catch (reason) { return { routes: [], parseError: errorMessage(reason) }; }
  }, [source?.xml]);
  const { busy: loading, error, refresh } = capture;
  const [filter, setFilter] = useSessionState(stateKey + ':filter', '');
  const [collapsed, setCollapsed] = useSessionState<string[]>(stateKey + ':collapsed', []);
  const [selectedId, setSelectedId] = useSessionState<string | null>(stateKey + ':route', null);
  const selected = routes.find(route => route.id === selectedId);
  const visibleRoutes = routes.filter(route => `${route.name} ${route.pattern} ${route.target}`.toLowerCase().includes(filter.toLowerCase()));
  return <div className="rest-openapi" aria-label="Rewriter explorer">
    <div className="rest-openapi-heading"><strong>Current app-server rewriter</strong><button aria-label="Refresh rewriter" disabled={loading || !connection.server} onClick={refresh}><RefreshCw size={14}/></button></div>
    {loading && <p className="rest-hint" role="status">Loading configured rewriter…</p>}
    <CaptureStatus {...capture}/>
    {parseError && <div className="rest-error" role="alert">{parseError}</div>}
    {source && <><p className="rest-hint"><strong>{source.path || 'No rewriter'}</strong><br/>{source.database} · root {source.root || '/'}</p>{source.message && <p className="rest-hint">{source.message}</p>}</>}
    {!connection.server && <p className="rest-hint">Choose an environment to inspect its rewriter.</p>}
    {routes.length > 0 && <><input className="rest-operation-filter" aria-label="Filter rewriter routes" placeholder="Find a route…" value={filter} onChange={event => setFilter(event.target.value)}/><p className="rest-hint">Request starters inferred from matching rules. Server-injected parameters are applied by the rewriter. Review the path and body before sending.</p>
      {selected && <div className="rest-route-details"><strong>Selected route</strong><code>{selected.pattern}</code><span>Dispatch: {selected.target || 'Pass through'}</span>{selected.notes.map((note, index) => <p key={index}>{note}</p>)}</div>}
      {[...new Set(visibleRoutes.map(route => route.group))].map(group => <div key={group}><button className="rest-api-group" aria-expanded={!collapsed.includes(group) || Boolean(filter)} onClick={() => setCollapsed(old => old.includes(group) ? old.filter(item => item !== group) : [...old, group])}>{collapsed.includes(group) && !filter ? <ChevronRight size={13}/> : <ChevronDown size={13}/>} {group}</button>{(!collapsed.includes(group) || filter) && visibleRoutes.filter(route => route.group === group).map(route => <div key={route.id}><button className="rest-request-item" disabled={disabled || !route.draft} title={`${route.pattern}\n${route.target}`} onClick={() => { setSelectedId(route.id); onOpen(route, source!.path); }}><span className={`rest-method method-${route.method.toLowerCase()}`}>{route.method}</span><span><strong>{route.path}</strong><small>{route.target || 'Pass through'}</small></span></button>{route.error && <p className="rest-hint">{route.pattern}: {route.error}</p>}</div>)}</div>)}
    </>}
    {source?.xml && !routes.length && !error && !parseError && <p className="rest-hint">No dispatch or REST request routes were found.</p>}
    {source?.xml && <details className="rest-rewriter-source"><summary>View rewriter XML</summary><pre>{source.xml}</pre></details>}
  </div>;
}
