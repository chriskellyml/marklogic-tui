import { useEffect, useMemo, useRef, useState } from 'react';
import { ArrowRight, Check, Code2, Copy, ExternalLink, Globe2, LoaderCircle, Plus, Search, Send, ShieldCheck, UserRound, Trash2, ChevronRight, ChevronDown, Folder, Pin, X, Star, Clock, BookOpen } from 'lucide-react';
import { errorMessage, type Connection, type ServerProfile } from './api';
import { QueryEditor } from './QueryEditor';
import { ExplorerCount } from './ExplorerCount';
import { LogsWorkspace } from './LogsWorkspace';
import { SidebarResizeHandle } from './SidebarResizeHandle';
import { buildRestRequest, createRestDraft, prettyRestBody, readRestDrafts, readRestCollections, restCollectionsKey, restApi, restBaseUrl, restEntry, restMethods, restStorageKey, restTemplates, type RestDraft, type RestEntry, type RestResponse, type RestCredentials } from './rest-api';
import { RewriterExplorer } from './RewriterExplorer';
import { OpenApiExplorer } from './OpenApiExplorer';
import type { ApiOperation } from './openapi';
import './rest.css';

const restPinsKey = 'workbench.rest-pins.v1';
const restFavoritesKey = 'workbench.rest-favorites.v1';
const restRecentKey = 'workbench.rest-recent.v1';
const restCollapsedKey = 'workbench.rest-collapsed.v1';
const restUsersKey = 'workbench.rest-users.v1';
type SavedRestUser = RestCredentials & { scope: string };
function isSavedRestUser(value: unknown): value is SavedRestUser {
  if (!value || typeof value !== 'object') return false;
  const item = value as Partial<SavedRestUser>;
  return typeof item.scope === 'string' && typeof item.username === 'string' && typeof item.password === 'string' &&
    (item.authType === undefined || ['basic', 'digest', 'digestbasic'].includes(item.authType));
}
function readSavedRestUsers(): SavedRestUser[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(restUsersKey) ?? '[]');
    return Array.isArray(value) ? value.filter(isSavedRestUser) : [];
  } catch { return []; }
}
function readPaths(key: string): string[] {
  try { const value: unknown = JSON.parse(localStorage.getItem(key) ?? '[]'); return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : []; } catch { return []; }
}
function ancestors(path: string): string[] { const parts = path.split('/'); return parts.map((_, i) => parts.slice(0, i + 1).join('/')); }
function inBranch(path: string, parent: string): boolean { return path === parent || path.startsWith(`${parent}/`); }

type SentResponse = { response: RestResponse; environment: string; method: string; username: string };

export function RestWorkspace({ active, connection, servers, onDirty, onServer = () => {}, onAppServer = () => {}, serverRevision = 0, modulesDatabase = 'Modules' }: {
  active: boolean; connection: Connection; servers: ServerProfile[]; onDirty: (dirty: boolean) => void;
  onServer?: (name: string) => void; onAppServer?: (name: string) => void; serverRevision?: number; modulesDatabase?: string;
}) {
  const [drafts, setDrafts] = useState(readRestDrafts);
  const [collections, setCollections] = useState(readRestCollections);
  const [favorites, setFavorites] = useState(() => readPaths(restFavoritesKey));
  const [recentRequests, setRecentRequests] = useState(() => readPaths(restRecentKey));
  const [pins, setPins] = useState(() => readPaths(restPinsKey));
  const [collapsed, setCollapsed] = useState(() => readPaths(restCollapsedKey));
  const [drag, setDrag] = useState<{kind:'request'|'collection'; id:string} | null>(null);
  const [treeError, setTreeError] = useState('');
  const [collectionName, setCollectionName] = useState('');
  const [setName, setSetName] = useState('');
  const [selectedId, setSelectedId] = useState(() => drafts[0]?.id ?? '');
  const [explorerTab, setExplorerTab] = useState<'requests' | 'openapi' | 'rewriter' | 'recent' | 'favorites'>('requests');
  const [filter, setFilter] = useState('');
  const [tab, setTab] = useState<'params' | 'headers' | 'body'>('params');
  const [outputMode, setOutputMode] = useState<'results' | 'logs' | 'split'>('results');
  const [logsOpened, setLogsOpened] = useState(false);
  const [responseTab, setResponseTab] = useState<'body' | 'headers'>('body');
  const [pretty, setPretty] = useState(true);
  const [responses, setResponses] = useState<Record<string, SentResponse>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [storageError, setStorageError] = useState('');
  const [userStoreError, setUserStoreError] = useState('');
  const [running, setRunning] = useState(false);
  const runningRef = useRef(false);
  const [sidebarWidth, setSidebarWidth] = useState(() => { try { return Number(localStorage.getItem('workbench.rest-sidebar-width')) || 420; } catch { return 420; } });
  const selected = drafts.find(item => item.id === selectedId) ?? drafts[0];
  const sent = selected ? responses[selected.id] : undefined;
  const error = selected ? errors[selected.id] : '';
  const baseUrl = restBaseUrl(servers.find(item => item.name === connection.server), connection.appServer);
  let logPort = '----';
  try { const url = new URL(baseUrl); logPort = url.port || (url.protocol === 'https:' ? '443' : '80'); } catch { /* No selected environment. */ }
  const formattedBody = useMemo(() => sent ? (pretty ? prettyRestBody(sent.response.body) : sent.response.body) : '', [sent, pretty]);
  const [copied, setCopied] = useState(false);
  const [curlExport, setCurlExport] = useState<{ path: string; script: string } | null>(null);
  const [curlError, setCurlError] = useState('');
  const [curlCopied, setCurlCopied] = useState(false);
  const [curlExporting, setCurlExporting] = useState(false);
  const [userOverride, setUserOverride] = useState<(RestCredentials & { scope: string }) | null>(null);
  const [savedUsers, setSavedUsers] = useState<SavedRestUser[]>(readSavedRestUsers);
  const [userDialog, setUserDialog] = useState(false);
  const [usernameDraft, setUsernameDraft] = useState('');
  const [passwordDraft, setPasswordDraft] = useState('');
  const [authTypeDraft, setAuthTypeDraft] = useState<'basic' | 'digest' | 'digestbasic'>('digestbasic');
  const scope = JSON.stringify([connection.server, connection.appServer, serverRevision]);
  const currentOverride = userOverride?.scope === scope ? userOverride : null;
  const savedUserScope = JSON.stringify([connection.server, connection.appServer]);
  const scopedSavedUsers = savedUsers.filter(item => item.scope === savedUserScope).sort((a, b) => a.username.localeCompare(b.username));
  const profile = servers.find(item => item.name === connection.server);
  const hasAuthHeader = selected?.headers.some(header => header.enabled && header.name.trim().toLowerCase() === 'authorization') ?? false;
  const currentUsername = currentOverride?.username ?? profile?.username ?? '';
  useEffect(() => { setUserOverride(null); setUserDialog(false); setPasswordDraft(''); }, [scope]);
  useEffect(() => {
    try { localStorage.setItem(restUsersKey, JSON.stringify(savedUsers)); setUserStoreError(''); }
    catch { setUserStoreError('Saved REST users could not be stored on this device.'); }
  }, [savedUsers]);
  function closeUserDialog() { setUserDialog(false); setPasswordDraft(''); }
  function editUser() { setUsernameDraft(currentOverride?.username ?? profile?.username ?? ''); setPasswordDraft(''); setAuthTypeDraft(currentOverride?.authType ?? 'digestbasic'); setUserDialog(true); }
  function saveAndUseUser() {
    const user = { scope: savedUserScope, username: usernameDraft.trim(), password: passwordDraft, ...(profile?.authType === 'application-level' ? { authType: authTypeDraft } : {}) };
    if (!user.username) return;
    setUserOverride({ ...user, scope });
    setSavedUsers(old => [...old.filter(item => item.scope !== savedUserScope || item.username.toLowerCase() !== user.username.toLowerCase()), user]);
    closeUserDialog();
  }


  useEffect(() => { try { localStorage.setItem(restFavoritesKey, JSON.stringify(favorites)); localStorage.setItem(restRecentKey, JSON.stringify(recentRequests)); } catch { /* Keep lists for this session. */ } }, [favorites, recentRequests]);
  useEffect(() => {
    if (selectedId && drafts.some(item => item.id === selectedId)) setRecentRequests(old => [selectedId, ...old.filter(id => id !== selectedId)].slice(0, 100));
  }, [selectedId]);
  useEffect(() => {
    try { localStorage.setItem(restStorageKey, JSON.stringify(drafts)); localStorage.setItem(restCollectionsKey, JSON.stringify(collections.filter(name => name !== 'Scratch'))); localStorage.setItem(restPinsKey, JSON.stringify(pins)); localStorage.setItem(restCollapsedKey, JSON.stringify(collapsed)); setStorageError(''); onDirty(false); }
    catch { setStorageError('Requests could not be saved locally. Keep this window open to retain your edits.'); onDirty(true); }
  }, [drafts, collections, pins, collapsed, onDirty]);

  useEffect(() => { const fit = () => setSidebarWidth(width => Math.max(220, Math.min(width, 900, window.innerWidth - 380))); fit(); window.addEventListener('resize', fit); return () => window.removeEventListener('resize', fit); }, []);
  useEffect(() => { try { localStorage.setItem('workbench.rest-sidebar-width', String(sidebarWidth)); } catch { /* Session only. */ } }, [sidebarWidth]);
  function openOperation(operation: ApiOperation, uri: string, kind = 'openapi') {
    if (!operation.draft) return;
    const id = `${kind}:${JSON.stringify([connection.server, connection.appServer, modulesDatabase, uri, operation.id])}`;
    if (!drafts.some(item => item.id === id)) setDrafts(old => [...old, { ...structuredClone(operation.draft!), id }]);
    setSelectedId(id); setTab('params');
  }
  function update(patch: Partial<RestDraft>) {
    if (selected) setDrafts(old => old.map(item => item.id === selected.id ? { ...item, ...patch } : item));
  }
  function add(template?: string) {
    const draft = { ...createRestDraft(template), collection: selected?.collection ?? 'Scratch' };
    setDrafts(old => [...old, draft]); setSelectedId(draft.id); setFilter(''); setTab('params');
  }
  function duplicate() {
    if (!selected) return;
    const copy = { ...selected, id: crypto.randomUUID(), name: `${selected.name} copy`,
      params: selected.params.map(entry => ({ ...entry, id: crypto.randomUUID() })), headers: selected.headers.map(entry => ({ ...entry, id: crypto.randomUUID() })), sets: structuredClone(selected.sets ?? []) };
    setDrafts(old => [...old, copy]); setSelectedId(copy.id); setFilter('');
  }
  function remove() {
    if (!selected) return;
    const remaining = drafts.filter(item => item.id !== selected.id);
    setDrafts(remaining); setSelectedId(remaining[0]?.id ?? ''); setFavorites(old => old.filter(id => id !== selected.id)); setRecentRequests(old => old.filter(id => id !== selected.id));
    setResponses(old => { const next = { ...old }; delete next[selected.id]; return next; });
    setErrors(old => { const next = { ...old }; delete next[selected.id]; return next; });
  }
  function saveSet() {
    if (!selected || !setName.trim()) return;
    const name = setName.trim();
    if ((selected.sets ?? []).some(item => item.name.toLowerCase() === name.toLowerCase())) return;
    update({ sets: [...(selected.sets ?? []), { id: crypto.randomUUID(), name, pathParams: structuredClone(selected.pathParams ?? []), params: structuredClone(selected.params), headers: structuredClone(selected.headers), body: selected.body, bodyEnabled: selected.bodyEnabled }] }); setSetName('');
  }
  function createCollection() {
    const name = collectionName.split('/').map(part => part.trim()).join('/');
    if (!name || name.split('/').some(part => !part || part === '.' || part === '..') || collections.some(item => item.toLowerCase() === name.toLowerCase())) { setTreeError('Use a unique collection path with non-empty folder names.'); return; }
    const paths = ancestors(name);
    setCollections(old => [...new Set([...old, ...paths])]); setCollapsed(old => old.filter(path => !paths.includes(path))); setTreeError(''); setCollectionName('');
    if (selected) update({ collection: name });
  }
  function dropOn(target: string | null) {
    if (!drag || running) return;
    if (drag.kind === 'request') {
      if (target) { setDrafts(old => old.map(item => item.id === drag.id ? { ...item, collection: target } : item)); setCollapsed(old => old.filter(path => path !== target)); }
    } else {
      const source = drag.id;
      if (source === 'Scratch' || (target && inBranch(target, source))) { setTreeError('Choose a destination outside the collection being moved.'); setDrag(null); return; }
      const destination = target ? `${target}/${source.split('/').at(-1)}` : source.split('/').at(-1)!;
      if (destination !== source && collections.some(path => inBranch(path, destination) && !inBranch(path, source))) { setTreeError('A collection already exists at that destination.'); setDrag(null); return; }
      const remap = (path: string) => inBranch(path, source) ? destination + path.slice(source.length) : path;
      setCollections(old => old.map(remap));
      setDrafts(old => old.map(item => ({ ...item, collection: item.collection ? remap(item.collection) : item.collection })));
      setPins(old => old.map(remap)); setCollapsed(old => old.map(remap).filter(path => path !== target));
    }
    setTreeError(''); setDrag(null);
  }
  const allCollections = [...new Set(['Scratch', ...collections.flatMap(ancestors), ...drafts.flatMap(item => ancestors(item.collection || 'Scratch'))])];
  function collectionTree(parent: string | null, depth = 0, rootPath?: string, pinnedTree = false): React.ReactNode {
    const children = allCollections.filter(path => rootPath ? path === rootPath : (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : null) === parent);
    return children.map(collection => {
      const isCollapsed = collapsed.includes(collection) && !filter;
      const entries = drafts.filter(item => (item.collection || 'Scratch') === collection && `${item.name} ${item.path} ${item.method}`.toLowerCase().includes(filter.toLowerCase()));
      return <div key={collection} id={`${pinnedTree ? 'rest-pinned-collection' : 'rest-collection'}-${collection}`} className="rest-collection"><div className="rest-collection-heading" style={{paddingLeft: 6 + depth * 14}} draggable={!running && collection !== 'Scratch'} onDragStart={event => { event.stopPropagation(); event.dataTransfer.effectAllowed = 'move'; event.dataTransfer.setData('text/plain', collection); setDrag({kind:'collection',id:collection}); }} onDragEnd={() => setDrag(null)} onDragOver={event => { if (drag) { event.preventDefault(); event.stopPropagation(); event.dataTransfer.dropEffect = 'move'; } }} onDrop={event => { event.preventDefault(); event.stopPropagation(); dropOn(collection); }}>
        <button aria-label={`Collection ${collection}`} aria-expanded={!isCollapsed} onClick={() => setCollapsed(old => old.includes(collection) ? old.filter(path => path !== collection) : [...old, collection])}>{isCollapsed ? <ChevronRight size={12}/> : <ChevronDown size={12}/>}<Folder size={13}/><strong>{collection.split('/').at(-1)}</strong></button><span>{drafts.filter(item => (item.collection || 'Scratch') === collection).length}</span><button className="rest-pin" aria-label={`Pin collection ${collection}`} title="Pin collection" disabled={pins.includes(collection)} onClick={() => setPins(old => [...old, collection])}><Pin size={13}/></button>
      </div>{!isCollapsed && <>{entries.map(item => <div key={item.id} className="rest-favorite-row"><button className="rest-request-item" style={{paddingLeft: 20 + depth * 14}} aria-pressed={item.id === selected?.id} disabled={running} draggable={!running} onDragStart={event => { event.stopPropagation(); event.dataTransfer.effectAllowed = 'move'; event.dataTransfer.setData('text/plain', item.id); setDrag({kind:'request',id:item.id}); }} onDragEnd={() => setDrag(null)} onClick={() => { setSelectedId(item.id); setCopied(false); }}><span className={`rest-method method-${item.method.toLowerCase()}`}>{item.method}</span><span><strong>{item.name || 'Untitled request'}</strong><small>{item.path}</small></span></button><button type="button" disabled={running} className={favorites.includes(item.id) ? 'is-favorite' : ''} aria-label={`${favorites.includes(item.id) ? 'Remove favorite' : 'Add favorite'} ${item.name || 'Untitled request'}`} title={favorites.includes(item.id) ? 'Remove from favorites' : 'Add to favorites'} onClick={() => setFavorites(old => old.includes(item.id) ? old.filter(id => id !== item.id) : [...old, item.id])}><Star size={14} fill={favorites.includes(item.id) ? 'currentColor' : 'none'}/></button></div>)}{collectionTree(collection, depth + 1, undefined, pinnedTree)}</>}</div>;
    });
  }
  async function send() {
    if (!selected || !baseUrl || runningRef.current) return;
    const id = selected.id;
    runningRef.current = true; setRunning(true); setErrors(old => ({ ...old, [id]: '' })); setCopied(false);
    const environment = `${connection.server} · ${connection.appServer ?? 'Profile default'}`;
    try {
      const request = buildRestRequest(selected);
      const credentials = currentOverride ? { username: currentOverride.username, password: currentOverride.password, authType: currentOverride.authType } : undefined;
      const response = credentials ? await restApi.execute(connection, request, credentials) : await restApi.execute(connection, request);
      setResponses(old => ({ ...old, [id]: { response, environment, method: request.method, username: hasAuthHeader && !currentOverride ? 'Authorization header' : currentUsername } }));
    } catch (reason) { setErrors(old => ({ ...old, [id]: errorMessage(reason) })); }
    finally { runningRef.current = false; setRunning(false); }
  }
  async function copyResponse() {
    try { await navigator.clipboard.writeText(formattedBody); setCopied(true); }
    catch { if (selected) setErrors(old => ({ ...old, [selected.id]: 'Could not copy the response to the clipboard.' })); }
  }
  async function exportCurl() {
    if (!selected || !baseUrl || curlExporting) return;
    setCurlError(''); setCurlCopied(false);
    setCurlExporting(true);
    try {
      const request = buildRestRequest(selected);
      const credentials = currentOverride ? { username: currentOverride.username, password: currentOverride.password, authType: currentOverride.authType } : undefined;
      setCurlExport(await restApi.exportCurl(connection, request, credentials));
    } catch (reason) { setCurlError(errorMessage(reason)); }
    finally { setCurlExporting(false); }
  }
  async function copyCurl() {
    if (!curlExport) return;
    try { await navigator.clipboard.writeText(curlExport.script); setCurlCopied(true); }
    catch { setCurlError('Could not copy the cURL script to the clipboard.'); }
  }

  return <section className="rest-workspace" aria-label="REST workspace" hidden={!active}
    style={{ gridTemplateColumns: `${sidebarWidth}px 6px minmax(0,1fr)` }}
    onKeyDown={event => { if (!userDialog && (event.metaKey || event.ctrlKey) && event.key === 'Enter') { event.preventDefault(); void send(); } }}>
    <aside className="rest-sidebar" aria-label="REST requests">
      <div className="rest-sidebar-title"><Globe2 size={18} /><strong>REST</strong><span>{drafts.length} requests</span></div>
      <nav className="explorer-tabs" aria-label="REST explorer views"><button aria-pressed={explorerTab === 'requests'} onClick={() => setExplorerTab('requests')}><Folder size={13}/> Requests</button><button aria-pressed={explorerTab === 'openapi'} onClick={() => setExplorerTab('openapi')}><BookOpen size={13}/> OpenAPI</button><button aria-pressed={explorerTab === 'rewriter'} onClick={() => setExplorerTab('rewriter')}><Code2 size={13}/> Rewriter</button><button aria-pressed={explorerTab === 'recent'} onClick={() => setExplorerTab('recent')}><Clock size={13}/> Recent <ExplorerCount count={recentRequests.filter(id => drafts.some(item => item.id === id)).length}/></button><button aria-pressed={explorerTab === 'favorites'} onClick={() => setExplorerTab('favorites')}><Star size={13}/> Favorites <ExplorerCount count={favorites.filter(id => drafts.some(item => item.id === id)).length}/></button></nav>
      <div className="rest-saved-explorer" hidden={explorerTab !== 'requests'}>
      <button className="rest-new" disabled={running} onClick={() => add()}><Plus size={15} />New request</button>
      <label className="rest-search"><Search size={14} /><input aria-label="Filter REST requests" placeholder="Find a request…" value={filter} onChange={event => setFilter(event.target.value)} /></label>
      <div className="rest-section-label">SAVED REQUESTS</div>
      <div className="rest-request-list">
        {pins.length > 0 && <section className="rest-pinned" aria-label="Pinned collections"><div className="rest-section-label">PINNED <button onClick={() => setPins([])}>Clear all</button></div>{pins.filter(path => allCollections.includes(path)).map(path => <div key={path} className="rest-pinned-group"><div className="rest-pinned-item"><span title={path}>{path}</span><button aria-label={`Unpin collection ${path}`} onClick={() => setPins(old => old.filter(item => item !== path))}><X size={12}/></button></div>{collectionTree(null, 0, path, true)}</div>)}</section>}
        <div className="rest-root-drop" onDragOver={event => { if (drag?.kind === 'collection') { event.preventDefault(); event.dataTransfer.dropEffect = 'move'; } }} onDrop={event => { event.preventDefault(); dropOn(null); }}>{drag?.kind === 'collection' ? 'Drop here to move to root' : 'COLLECTIONS'}</div>
        {collectionTree(null)}
        {filter && !drafts.some(item => `${item.name} ${item.path} ${item.method}`.toLowerCase().includes(filter.toLowerCase())) && <p className="rest-hint">No matching requests.</p>}
      </div>
      {treeError && <div className="rest-error" role="alert">{treeError}</div>}
      <form className="rest-create-collection" onSubmit={event => { event.preventDefault(); createCollection(); }}><input aria-label="New collection name" placeholder="Collection or folder/name" value={collectionName} onChange={event => setCollectionName(event.target.value)} /><button type="submit" disabled={running || !collectionName.trim()} aria-label="Create collection"><Plus size={14}/></button></form>
      <div className="rest-templates"><div className="rest-section-label">MARKLOGIC STARTERS</div>
        {restTemplates.map(template => <button key={template.id} disabled={running} onClick={() => add(template.id)}><Plus size={13} /><span>{template.name}</span><ArrowRight size={12} /></button>)}
      </div>
      </div><div className="rest-api-explorer" hidden={explorerTab !== 'openapi'}><OpenApiExplorer key={JSON.stringify([connection.server, connection.appServer, modulesDatabase, serverRevision])} active={active} connection={connection} database={modulesDatabase} disabled={running} onOpen={openOperation}/></div>
      <div className="rest-api-explorer" hidden={explorerTab !== 'rewriter'}><RewriterExplorer key={JSON.stringify([connection.server, connection.appServer, serverRevision])} active={active && explorerTab === 'rewriter'} connection={connection} disabled={running} onOpen={(route, path) => openOperation(route, path, 'rewriter')}/></div>
      {(explorerTab === 'recent' || explorerTab === 'favorites') && <div className="rest-request-list" aria-label={explorerTab === 'recent' ? 'Recent requests' : 'Favorite requests'}>{(explorerTab === 'recent' ? recentRequests : favorites).map(id => drafts.find(item => item.id === id)).filter((item): item is RestDraft => Boolean(item)).map(item => <div className="rest-favorite-row" key={item.id}><button className="rest-request-item" aria-pressed={item.id === selected?.id} disabled={running} onClick={() => { setSelectedId(item.id); setCopied(false); }}><span className={`rest-method method-${item.method.toLowerCase()}`}>{item.method}</span><span><strong>{item.name || 'Untitled request'}</strong><small>{item.collection || 'Scratch'} · {item.path}</small></span></button><button aria-label={`${favorites.includes(item.id) ? 'Remove favorite' : 'Add favorite'} ${item.name}`} onClick={() => setFavorites(old => old.includes(item.id) ? old.filter(id => id !== item.id) : [...old, item.id])}><Star size={14} fill={favorites.includes(item.id) ? 'currentColor' : 'none'}/></button></div>)}{!(explorerTab === 'recent' ? recentRequests : favorites).some(id => drafts.some(item => item.id === id)) && <p className="rest-hint">{explorerTab === 'recent' ? 'No recent requests.' : 'Star a request to add it here.'}</p>}</div>}
      <p className="rest-sidebar-note">Requests stay on this machine.<br />Switch environments to reuse them.</p>
    </aside>
    <SidebarResizeHandle label="Resize REST sidebar" width={sidebarWidth} defaultWidth={420} onChange={width => setSidebarWidth(Math.max(220, Math.min(width, 900, window.innerWidth - 380)))} />
    <main className="rest-main">
      {storageError && <div className="rest-error" role="alert">{storageError}</div>}
        <div className="rest-auth"><ShieldCheck size={13} /><span>{hasAuthHeader ? (currentOverride ? 'Remove the Authorization header to use the override' : 'Request supplies an Authorization header') : currentOverride ? 'REST credentials overridden' : profile?.authType === 'application-level' ? 'Application-level authentication' : 'Authentication inherited from the environment'}</span><button className={`rest-user ${currentOverride ? 'rest-user-override' : ''}`} type="button" aria-label={currentOverride ? `REST user override: ${currentUsername}` : `REST user: ${currentUsername || 'No user selected'}`} disabled={!connection.server || running} onClick={editUser}><UserRound size={13}/>{currentUsername || 'No user selected'}{currentOverride && <strong>OVERRIDE</strong>}</button></div>
        {userDialog && <div className="scratch-save-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) closeUserDialog(); }} onKeyDown={event => { if (event.key === 'Escape') { event.stopPropagation(); closeUserDialog(); } }}><form className="scratch-save-dialog rest-user-dialog" role="dialog" aria-modal="true" aria-labelledby="rest-user-dialog-title" onSubmit={event => { event.preventDefault(); saveAndUseUser(); }}><h2 id="rest-user-dialog-title">REST user</h2><p>Override REST credentials for {connection.server} · {connection.appServer || 'Profile default'}. Saved users stay on this device for this connection until you delete them. The override applies until you switch back to the environment user.</p>
          {scopedSavedUsers.length > 0 && <section className="rest-saved-users" aria-label="Saved REST users"><strong>Saved users</strong>{scopedSavedUsers.map(user => <div key={user.username}><button type="button" onClick={() => { setUsernameDraft(user.username); setPasswordDraft(user.password); setAuthTypeDraft(user.authType ?? 'digestbasic'); }}>{user.username}</button><button type="button" aria-label={`Delete saved REST user ${user.username}`} title={`Delete ${user.username}`} onClick={() => setSavedUsers(old => old.filter(item => !(item.scope === user.scope && item.username === user.username)))}><Trash2 size={13}/></button></div>)}</section>}
          {userStoreError && <p role="alert">{userStoreError}</p>}
          <label>Username<input autoFocus required autoComplete="off" aria-label="REST override username" value={usernameDraft} onChange={event => setUsernameDraft(event.target.value)} /></label><label>Password<input type="password" autoComplete="new-password" aria-label="REST override password" value={passwordDraft} onChange={event => setPasswordDraft(event.target.value)} /></label>{profile?.authType === 'application-level' && <label>Authentication<select aria-label="REST override authentication" value={authTypeDraft} onChange={event => setAuthTypeDraft(event.target.value as typeof authTypeDraft)}><option value="digestbasic">Digest / Basic</option><option value="digest">Digest</option><option value="basic">Basic</option></select><span>The app server must accept the selected HTTP authentication method.</span></label>}<div><button type="button" onClick={() => { setUserOverride(null); closeUserDialog(); }}>Use environment user</button><button type="button" onClick={closeUserDialog}>Cancel</button><button className="run-button" type="submit" disabled={!usernameDraft.trim()}>Use this user</button></div></form></div>}
      {selected ? <>
        <header className="rest-heading"><input aria-label="Request name" value={selected.name} disabled={running} onChange={event => update({ name: event.target.value })} />
          <label className="rest-collection-select">Collection <select aria-label="Request collection" value={selected.collection || 'Scratch'} disabled={running} onChange={event => update({ collection: event.target.value })}>{allCollections.map(name => <option key={name}>{name}</option>)}</select></label>
          <button type="button" aria-label={favorites.includes(selected.id) ? "Remove request from favorites" : "Add request to favorites"} title={favorites.includes(selected.id) ? "Remove from favorites" : "Add to favorites"} onClick={() => setFavorites(old => old.includes(selected.id) ? old.filter(id => id !== selected.id) : [...old, selected.id])}><Star size={15} fill={favorites.includes(selected.id) ? "currentColor" : "none"}/></button><span className="rest-save-state">{storageError ? 'Not saved' : <><Check size={12} />Saved locally</>}</span>
          <button title="Duplicate request" aria-label="Duplicate request" disabled={running} onClick={duplicate}><Copy size={15} /> Duplicate</button>
          <button title="Delete request" aria-label="Delete request" disabled={running} onClick={remove}><Trash2 size={15} /> Delete</button>
        </header>
        <div className="rest-environment"><Globe2 size={14} /><span>BASE URL</span><code aria-label="REST base URL">{baseUrl || 'Choose an environment'}</code><span className="rest-environment-source">From current environment</span></div>
        <form className="rest-url-bar" onSubmit={event => { event.preventDefault(); void send(); }}>
          <select aria-label="HTTP method" value={selected.method} className={`rest-method method-${selected.method.toLowerCase()}`} disabled={running} onChange={event => update({ method: event.target.value as RestDraft['method'] })}>{restMethods.map(method => <option key={method}>{method}</option>)}</select>
          <input aria-label="Request path" placeholder="/v1/search" value={selected.path} disabled={running} onChange={event => update({ path: event.target.value })} spellCheck={false} />
          <button className="rest-export-curl" type="button" disabled={running || curlExporting || !baseUrl} title="Export this request as a cURL script" onClick={() => void exportCurl()}>{curlExporting ? <LoaderCircle className="spin" size={14}/> : <Code2 size={14}/>}cURL</button>
          <button className="rest-send" type="submit" disabled={running || !baseUrl} title="Send request (⌘ Enter / Ctrl Enter)">{running ? <LoaderCircle className="spin" size={14} /> : <Send size={14} />}{running ? 'Sending…' : 'Send'}</button>
        </form>
        {curlError && <div className="rest-error" role="alert">{curlError}</div>}
        {curlExport && <div className="scratch-save-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) setCurlExport(null); }} onKeyDown={event => { if (event.key === 'Escape') { event.stopPropagation(); setCurlExport(null); } }}><section className="scratch-save-dialog rest-curl-dialog" role="dialog" aria-modal="true" aria-labelledby="rest-curl-title"><h2 id="rest-curl-title">cURL script</h2><p>This script is saved with private permissions and includes the current request and authentication.</p><button className="rest-curl-file-link" type="button" onClick={() => void restApi.openCurl(curlExport.path).catch(reason => setCurlError(errorMessage(reason)))} title="Open the generated script"><span>{curlExport.path}</span><ExternalLink size={13}/></button><textarea aria-label="Generated cURL script" readOnly spellCheck={false} value={curlExport.script}/>{curlError && <p role="alert">{curlError}</p>}<div><button type="button" onClick={() => { setCurlExport(null); setCurlError(''); }}>Close</button><button className="run-button" type="button" onClick={() => void copyCurl()}>{curlCopied ? <><Check size={13}/>Copied</> : <><Copy size={13}/>Copy script</>}</button></div></section></div>}
        <section className="rest-composer" aria-label="Request editor">
          <nav className="rest-tabs" aria-label="Request sections">
            {(['params', 'headers', 'body'] as const).map(value => <button key={value} aria-pressed={tab === value} onClick={() => setTab(value)}>{value === 'params' ? 'Params' : value === 'headers' ? 'Headers' : 'Body'}<span>{value === 'body' ? (selected.bodyEnabled ? 'ON' : 'OFF') : selected[value].filter(entry => entry.enabled && entry.name.trim()).length + (value === 'params' ? (selected.pathParams?.length ?? 0) : 0)}</span></button>)}
          </nav>
          <div className="rest-composer-layout"><div className="rest-request-content">
            {tab === 'body' ? <><label className="rest-body-toggle"><input type="checkbox" checked={selected.bodyEnabled} disabled={running} onChange={event => update({ bodyEnabled: event.target.checked })} />Send request body<span>Set Content-Type in Headers.</span></label>
              <div className="rest-body-editor"><QueryEditor label="Request body" source={selected.body} language="text" disabled={running || !selected.bodyEnabled} onChange={body => update({ body })} /></div></> :
              <>{tab === 'params' && Boolean(selected.pathParams?.length) && <><div className="rest-section-label">PATH PARAMETERS</div><EntryTable label="path parameter" entries={selected.pathParams ?? []} disabled={running} onChange={pathParams => update({ pathParams })}/></>}<EntryTable key={tab} label={tab === 'params' ? 'parameter' : 'header'} entries={selected[tab]} disabled={running} onChange={entries => update({ [tab]: entries })} /></>}
          </div><aside className="rest-sets" aria-label="Saved request sets"><strong>Saved sets</strong><p>Save params, headers and body together.</p>
            <form onSubmit={event => { event.preventDefault(); saveSet(); }}><input aria-label="Set name" placeholder="Set name" value={setName} onChange={event => setSetName(event.target.value)} /><button disabled={running || !setName.trim() || (selected.sets ?? []).some(item => item.name.toLowerCase() === setName.trim().toLowerCase())} type="submit">Save</button></form>
            {(selected.sets ?? []).map(set => <div className="rest-set-row" key={set.id}><button disabled={running} title={`Load ${set.name}`} onClick={() => update({ pathParams: structuredClone(set.pathParams ?? []), params: structuredClone(set.params), headers: structuredClone(set.headers), body: set.body, bodyEnabled: set.bodyEnabled })}>{set.name}</button><button disabled={running} aria-label={`Update set ${set.name}`} title="Update from current request" onClick={() => update({ sets: (selected.sets ?? []).map(item => item.id === set.id ? { ...item, pathParams: structuredClone(selected.pathParams ?? []), params: structuredClone(selected.params), headers: structuredClone(selected.headers), body: selected.body, bodyEnabled: selected.bodyEnabled } : item) })}><Check size={13}/></button><button disabled={running} aria-label={`Delete set ${set.name}`} onClick={() => update({ sets: (selected.sets ?? []).filter(item => item.id !== set.id) })}><Trash2 size={13}/></button></div>)}
          </aside></div>
        </section>
        <section className="rest-response" aria-label="REST response" aria-busy={running}>
          <header className="rest-response-heading"><nav className="query-output-tabs" aria-label="REST output views">{(['results', 'logs', 'split'] as const).map(mode => <button key={mode} aria-label={mode === 'results' ? 'Show REST response' : mode === 'logs' ? 'Show REST logs' : 'Split response and logs'} aria-pressed={outputMode === mode} onClick={() => { setOutputMode(mode); if (mode !== 'results') setLogsOpened(true); }}>{mode === 'results' ? 'Results' : mode === 'logs' ? 'Logs' : 'Split'}</button>)}</nav>{sent && <><span className={`rest-status ${sent.response.status < 400 ? 'success' : 'failure'}`}>{sent.response.status} {sent.response.statusText}</span><span>{sent.response.elapsedMs.toLocaleString()} ms</span><span>{formatSize(sent.response.size)}</span></>}
            {running && <span role="status">Sending request…</span>}
          </header>
          <div className={`query-output-layout output-${outputMode}`}><div className="query-result-pane" hidden={outputMode === 'logs'}>
          {error && <div className="rest-error" role="alert">{error}</div>}
          {sent ? <>
            <div className="rest-response-origin" title={sent.response.url}>{sent.method} {sent.response.url}<span>{sent.environment}{sent.username ? ` · ${sent.username}` : ''}</span></div>
            {sent.response.truncated && <p className="rest-error">Response truncated at 10 MiB.</p>}
            <nav className="rest-tabs" aria-label="Response sections"><button aria-pressed={responseTab === 'body'} onClick={() => setResponseTab('body')}>Body</button><button aria-pressed={responseTab === 'headers'} onClick={() => setResponseTab('headers')}>Headers<span>{sent.response.headers.length}</span></button>
              <div className="rest-response-actions">{responseTab === 'body' && <><button aria-pressed={pretty} onClick={() => { setPretty(value => !value); setCopied(false); }}>{pretty ? 'Pretty' : 'Raw'}</button><button aria-label="Copy response body" onClick={() => void copyResponse()}>{copied ? <Check size={13} /> : <Copy size={13} />}{copied ? 'Copied' : 'Copy'}</button></>}</div>
            </nav>
            <div className="rest-response-content">{responseTab === 'headers' ? <table className="rest-response-headers"><thead><tr><th>Header</th><th>Value</th></tr></thead><tbody>{sent.response.headers.map(([name, value], index) => <tr key={index}><td>{name}</td><td>{value}</td></tr>)}</tbody></table> : formattedBody ? <pre aria-label="Response body">{formattedBody}</pre> : <p className="rest-hint">This response has no body.</p>}</div>
          </> : <div className="rest-response-empty"><Send size={24} /><strong>Ready to make a request</strong><p>Enter a relative path and press Send.<br />Your current environment provides the host, port and authentication.</p><kbd>⌘ Enter / Ctrl Enter</kbd></div>}
          </div>{logsOpened && <div className="query-log-pane" hidden={outputMode === 'results'}><LogsWorkspace key={`rest-logs:${serverRevision}`} embedded preferredPort={logPort} refreshInterval={10000} active={active} servers={servers} connection={connection} disabled={false} onServer={onServer} onAppServer={onAppServer}/></div>}</div>
        </section>
      </> : <div className="rest-response-empty"><Globe2 size={32} /><strong>Your REST workspace</strong><p>Create a request or choose a MarkLogic starter.</p><button className="rest-new" onClick={() => add()}><Plus size={15} />New request</button></div>}
    </main>
  </section>;
}

function EntryTable({ entries, onChange, label, disabled }: { entries: RestEntry[]; onChange: (entries: RestEntry[]) => void; label: string; disabled: boolean }) {
  const change = (id: string, patch: Partial<RestEntry>) => onChange(entries.map(entry => entry.id === id ? { ...entry, ...patch } : entry));
  return <div className="rest-entries"><table><thead><tr><th aria-label="Enabled" /><th>Name</th><th>Value</th><th /></tr></thead><tbody>
    {entries.map((entry, index) => <tr key={entry.id} className={entry.enabled ? '' : 'rest-disabled-entry'}>
      <td><input type="checkbox" aria-label={`Enable ${label} ${index + 1}`} checked={entry.enabled} disabled={disabled} onChange={event => change(entry.id, { enabled: event.target.checked })} /></td>
      <td><input aria-label={`${label} ${index + 1} name`} placeholder="Name" value={entry.name} disabled={disabled} onChange={event => change(entry.id, { name: event.target.value })} spellCheck={false} /></td>
      <td><input aria-label={`${label} ${index + 1} value`} placeholder={entry.required ? 'Required' : 'Value'} title={entry.description} value={entry.value} disabled={disabled} onChange={event => change(entry.id, { value: event.target.value })} spellCheck={false} /></td>
      <td><button aria-label={`Remove ${label} ${index + 1}`} disabled={disabled} onClick={() => onChange(entries.filter(item => item.id !== entry.id))}><Trash2 size={13} /></button></td>
    </tr>)}
  </tbody></table><button className="rest-add-entry" disabled={disabled} onClick={() => onChange([...entries, restEntry()])}><Plus size={13} />Add {label}</button></div>;
}

function formatSize(bytes: number): string { return bytes < 1024 ? `${bytes} B` : bytes < 1024 * 1024 ? `${(bytes / 1024).toFixed(1)} KiB` : `${(bytes / (1024 * 1024)).toFixed(1)} MiB`; }
