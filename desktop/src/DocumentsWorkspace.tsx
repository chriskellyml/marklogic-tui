import { useEffect, useMemo, useState } from 'react';
import { ChevronDown, ChevronLeft, ChevronRight, FileText, Folder, FolderOpen, RefreshCw, Trash2, ScanSearch, X } from 'lucide-react';
import { api, errorMessage, type Connection } from './api';
import { useHostCapture } from './host-capture';
import { documentsApi, readCollectionSets, type CollectionFilter, type CollectionSet, type DocumentDetail, type DocumentPage, type CollectionPage, type UriCursor } from './documents-api';
import { DocumentViewer } from './DocumentViewer';
import { SidebarResizeHandle } from './SidebarResizeHandle';
import { useSessionKey, useSessionState } from './workbench-sessions';
import './documents.css';

const emptyFilter: CollectionFilter = { collections: [], mode: 'any' };

export function DocumentsWorkspace({ active, connection }: { active: boolean; connection: Connection }) {
  const widthKey = useSessionKey('workbench.documents-sidebar-width');
  const [visited, setVisited] = useState(active);
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    try { return Number(localStorage.getItem(widthKey)) || 360; } catch { return 360; }
  });
  function resizeSidebar(width: number) { setSidebarWidth(Math.max(240, Math.min(width, 900, window.innerWidth - 380))); }
  useEffect(() => {
    const fit = () => resizeSidebar(sidebarWidth);
    fit(); window.addEventListener('resize', fit);
    return () => window.removeEventListener('resize', fit);
  }, [sidebarWidth]);
  useEffect(() => { try { localStorage.setItem(widthKey, String(sidebarWidth)); } catch { /* session only */ } }, [sidebarWidth, widthKey]);
  useEffect(() => { if (active) setVisited(true); }, [active]);
  return <section className="documents-workspace" hidden={!active} aria-label="Documents workspace" style={{gridTemplateColumns: `${sidebarWidth}px 6px minmax(0,1fr)`}}>
    {visited && (connection.server && connection.database.trim() ?
      <DatabaseExplorer key={JSON.stringify(connection)} active={active} connection={connection} sidebarWidth={sidebarWidth} onResize={resizeSidebar} /> :
      <p className="document-empty">Choose a server and database in the connection menu to explore documents.</p>)}
  </section>;
}

function DatabaseExplorer({ active, connection, sidebarWidth, onResize }: { active: boolean; connection: Connection; sidebarWidth: number; onResize: (width: number) => void }) {
  const [database, setDatabase] = useState(connection.database);
  const selectedConnection = useMemo(() => ({ ...connection, database }), [connection, database]);
  const catalog = useHostCapture(active, connection, api.listDatabases);
  const databases = [...new Set([database, connection.database, ...(catalog.capture?.data ?? [])])];
  return <DocumentExplorer key={JSON.stringify(selectedConnection)} connection={selectedConnection} sidebarWidth={sidebarWidth} onResize={onResize}
    databases={databases} onDatabaseChange={setDatabase} catalogError={catalog.error?.message} retryCatalog={catalog.refresh} />;
}

function DocumentExplorer({ connection, sidebarWidth, onResize, databases, onDatabaseChange, catalogError, retryCatalog }: {
  connection: Connection; sidebarWidth: number; onResize: (width: number) => void; databases: string[];
  onDatabaseChange: (database: string) => void; catalogError?: string; retryCatalog: () => void;
}) {
  const stateKey = `documents:${JSON.stringify(connection)}`;
  const [filter, setFilter] = useSessionState<CollectionFilter>(stateKey + ':filter', emptyFilter);
  const [revision, setRevision] = useState(0);
  const [uri, setUri] = useSessionState(stateKey + ':uri', '');
  const [document, setDocument] = useState<DocumentDetail | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [focus, setFocus] = useSessionState(stateKey + ':folder', '');
  const [deleting, setDeleting] = useState(false);
  const [treeError, setTreeError] = useState('');
  async function remove(path: string, folder: boolean) {
    if (deleting) return;
    setDeleting(true); setTreeError('');
    try {
      if (!await documentsApi.confirmDelete(connection, path, folder)) return;
      await documentsApi.delete(connection, path, folder);
      if (uri && (folder ? uri.startsWith(path) : uri === path)) setUri('');
      if (folder && focus.startsWith(path)) setFocus('');
      setRevision(value => value + 1);
    } catch (reason) {
      setTreeError(errorMessage(reason));
      // Refresh even after an error: a timed-out request may have committed.
      setRevision(value => value + 1);
    } finally { setDeleting(false); }
  }
  function focusPath(path: string) {
    setFocus(path);
    if (path && uri && !uri.startsWith(path)) setUri('');
  }
  useEffect(() => {
    let alive = true;
    setDocument(null); setError(''); setLoading(Boolean(uri));
    if (uri) void documentsApi.inspect(connection, uri).then(value => { if (alive) setDocument(value); })
      .catch(reason => { if (alive) setError(errorMessage(reason)); }).finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [connection, uri, revision]);
  const treeKey = JSON.stringify([filter, focus]);
  return <>
    <aside className="document-explorer" aria-label="Document explorer">
      <div className="document-explorer-heading"><div><strong>Documents</strong><span>{connection.server}</span><select aria-label="Document database" value={connection.database} onChange={event => onDatabaseChange(event.target.value)}>{databases.map(database => <option key={database} value={database}>{database}</option>)}</select></div><button aria-label="Refresh documents" title="Refresh documents and collections" onClick={() => setRevision(value => value + 1)}><RefreshCw size={15} /></button></div>
      {catalogError && <div className="document-warning">
        <details>
          <summary>Could not load databases</summary>
          <p>Check the Manage endpoint in Connection.</p>
          <p>{catalogError}</p>
        </details>
        <button onClick={retryCatalog}>Retry database discovery</button>
      </div>}
      <CollectionSets connection={connection} filter={filter} revision={revision} onChange={next => { setFilter(next); setUri(''); }} />
      <div className="document-tree-heading"><strong>URI PATHS</strong><span>100 per page</span></div>
      {treeError && <div className="document-error" role="alert">{treeError}<button onClick={() => setTreeError('')}>Dismiss</button></div>}
      {deleting && <p className="document-hint" role="status">Deleting documents…</p>}
      <div className="document-tree">{focus && <div className="document-path-row focused-path"><span title={focus}><FolderOpen size={14}/>{focus}</span><button className="document-path-action" aria-label={`Unfocus ${focus}`} title="Show all paths" onClick={() => focusPath('')} disabled={deleting}><ScanSearch size={14}/></button><button className="document-path-action" aria-label={`Delete folder ${focus}`} title="Delete folder and all documents" onClick={() => void remove(focus, true)} disabled={deleting}><Trash2 size={14}/></button></div>}<UriBranch key={treeKey} connection={connection} prefix={focus} filter={filter} revision={revision} selected={uri} onSelect={setUri} onFocus={focusPath} onDelete={remove} disabled={deleting} /></div>
    </aside>
    <SidebarResizeHandle width={sidebarWidth} onChange={onResize} label="Resize documents sidebar" defaultWidth={360} />
    <main className="document-viewer" aria-label="Document viewer" aria-busy={loading}>
      {error ? <div className="document-error" role="alert">{error}<button onClick={() => setRevision(value => value + 1)}>Retry</button></div> : loading ? <p className="document-empty" role="status">Loading {uri}…</p> :
        document ? <DocumentViewer stateKey={stateKey + ":viewer"} key={document.uri} document={document} /> : <div className="document-empty"><FileText size={32} /><strong>Explore your database</strong><p>Expand a URI path and select a document to view its content, collections, metadata and properties.</p></div>}
    </main>
  </>;
}

function UriBranch({ connection, prefix, filter, revision, selected, onSelect, onFocus, onDelete, disabled }: {
  connection: Connection; prefix: string; filter: CollectionFilter; selected: string; onSelect: (uri: string) => void;
  revision: number;
  onFocus: (uri: string) => void; onDelete: (uri: string, folder: boolean) => void; disabled: boolean;
}) {
  const stateKey = `documents.tree:${JSON.stringify([connection, filter, prefix])}`;
  const [cursors, setCursors] = useSessionState<(UriCursor | null)[]>(stateKey + ':cursors', [null]);
  const [page, setPage] = useState<DocumentPage | null>(null);
  const [expanded, setExpanded] = useSessionState<string[]>(stateKey + ':expanded', []);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [retry, setRetry] = useState(0);
  const cursor = cursors[cursors.length - 1];
  useEffect(() => {
    let alive = true;
    setLoading(true); setError('');
    void documentsApi.browse(connection, prefix, cursor, filter).then(value => {
      if (!alive) return;
      if (!value.entries.length && cursors.length > 1) { setCursors(old => old.slice(0, -1)); return; }
      setPage(value);
    })
      .catch(reason => { if (alive) setError(errorMessage(reason)); }).finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [connection, prefix, cursor, filter, retry, revision]);
  return <div className="document-branch" aria-busy={loading}>
    {loading && !page ? <p className="document-hint" role="status">Loading paths…</p> : error ? <div className="document-error" role="alert">{error}<button onClick={() => setRetry(value => value + 1)}>Retry paths</button></div> :
      page && <><ul aria-label={prefix || 'Top level URI paths'}>{page.entries.map(entry => <li key={`${entry.kind}:${entry.uri}`}>
        {entry.kind === 'folder' ? <><div className="document-path-row"><button className="document-path folder" aria-label={`Expand ${entry.uri}`} aria-expanded={expanded.includes(entry.uri)} title={entry.uri} onClick={() => setExpanded(old => old.includes(entry.uri) ? old.filter(path => path !== entry.uri) : [...old, entry.uri])}>
          {expanded.includes(entry.uri) ? <ChevronDown size={12} /> : <ChevronRight size={12} />}{expanded.includes(entry.uri) ? <FolderOpen size={14} /> : <Folder size={14} />}<span>{entry.name}</span></button><button className="document-path-action" aria-label={`Focus ${entry.uri}`} title="Focus this folder" disabled={disabled} onClick={() => onFocus(entry.uri)}><ScanSearch size={14}/></button><button className="document-path-action" aria-label={`Delete folder ${entry.uri}`} title="Delete folder and all documents" disabled={disabled} onClick={() => onDelete(entry.uri, true)}><Trash2 size={14}/></button></div>
          {expanded.includes(entry.uri) && <UriBranch connection={connection} prefix={entry.uri} filter={filter} revision={revision} selected={selected} onSelect={onSelect} onFocus={onFocus} onDelete={onDelete} disabled={disabled} />}</> :
          <div className="document-path-row"><button className="document-path" aria-pressed={selected === entry.uri} title={entry.uri} onClick={() => onSelect(entry.uri)}><FileText size={14} /><span>{entry.name}</span></button><button className="document-path-action" aria-label={`Delete document ${entry.uri}`} title="Delete document" disabled={disabled} onClick={() => onDelete(entry.uri, false)}><Trash2 size={14}/></button></div>}
      </li>)}</ul>{!page.entries.length && <p className="document-hint">{filter.collections.length ? 'No documents match this collection set.' : 'No documents in this path.'}</p>}
        {(cursors.length > 1 || page.nextCursor) && <div className="document-pagination"><button aria-label={`Previous paths in ${prefix || 'root'}`} disabled={loading || cursors.length === 1} onClick={() => { setPage(null); setExpanded([]); setCursors(old => old.slice(0, -1)); }}><ChevronLeft size={14} /></button><span>Page {cursors.length} · {page.entries.length} entries</span><button aria-label={`Next paths in ${prefix || 'root'}`} disabled={loading || !page.nextCursor} onClick={() => { setPage(null); setExpanded([]); setCursors(old => [...old, page.nextCursor]); }}><ChevronRight size={14} /></button></div>}</>}
  </div>;
}

function CollectionSets({ connection, filter, revision, onChange }: { connection: Connection; filter: CollectionFilter; revision: number; onChange: (filter: CollectionFilter) => void }) {
  const storageKey = `workbench.collection-sets.v1:${JSON.stringify(connection)}`;
  const [sets, setSets] = useState<CollectionSet[]>(() => readCollectionSets(storageKey));
  const [name, setName] = useState('');
  const [saved, setSaved] = useSessionState(storageKey + ':selected', '');
  const [search, setSearch] = useState('');
  const [searchPrefix, setSearchPrefix] = useState('');
  const [afters, setAfters] = useState<(string | null)[]>([null]);
  const [page, setPage] = useState<CollectionPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [storageError, setStorageError] = useState('');
  const after = afters[afters.length - 1];
  useEffect(() => {
    let alive = true; setLoading(true); setError(''); setPage(null);
    void documentsApi.collections(connection, searchPrefix, after).then(value => { if (alive) setPage(value); })
      .catch(reason => { if (alive) setError(errorMessage(reason)); }).finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [connection, searchPrefix, after, revision]);
  function updateSets(next: CollectionSet[]) {
    setSets(next); setStorageError('');
    try { localStorage.setItem(storageKey, JSON.stringify(next)); } catch { setStorageError('Could not save collection sets. This set is available for this session.'); }
  }
  function change(next: CollectionFilter) { setSaved(''); onChange(next); }
  function toggle(uri: string) {
    if (!filter.collections.includes(uri) && filter.collections.length >= 100) return;
    change({ ...filter, collections: filter.collections.includes(uri) ? filter.collections.filter(value => value !== uri) : [...filter.collections, uri] });
  }
  return <section className="collection-sets" aria-label="Collection sets">
    <div className="document-tree-heading"><strong>COLLECTION SETS</strong><button disabled={!filter.collections.length} onClick={() => { setSaved(''); setName(''); onChange(emptyFilter); }}>Clear filters</button></div>
    <div className="collection-saved"><select aria-label="Saved collection sets" value={saved} onChange={event => { const set = sets.find(value => value.name === event.target.value); setSaved(set?.name ?? ''); setName(set?.name ?? ''); onChange(set ? { collections: set.collections, mode: set.mode } : emptyFilter); }}><option value="">{filter.collections.length ? 'Custom set' : 'All documents'}</option>{sets.map(set => <option key={set.name} value={set.name}>{set.name}</option>)}</select><button aria-label="Delete saved collection set" disabled={!saved} onClick={() => { updateSets(sets.filter(set => set.name !== saved)); setSaved(''); }}>Delete</button></div>
    <div className="collection-mode" role="group" aria-label="Combine collections"><button aria-pressed={filter.mode === 'any'} onClick={() => change({ ...filter, mode: 'any' })}>Any (OR)</button><button aria-pressed={filter.mode === 'all'} onClick={() => change({ ...filter, mode: 'all' })}>All (AND)</button><span>{filter.collections.length} selected</span></div>
    <p className="document-hint">{filter.collections.length ? filter.mode === 'any' ? 'Documents in any selected collection.' : 'Documents in every selected collection.' : 'All documents. Select collections to limit the tree.'}</p>
    <div className="collection-selected">{filter.collections.map(uri => <button key={uri} title={`Remove ${uri}`} onClick={() => toggle(uri)}><span>{uri}</span><X size={11} /></button>)}</div>
    <details className="collection-picker"><summary>Choose collections</summary><form onSubmit={event => { event.preventDefault(); setSearchPrefix(search); setAfters([null]); }}><input aria-label="Collection prefix or URI" value={search} placeholder="Collection prefix or exact URI…" onChange={event => setSearch(event.target.value)} /><button type="submit">Find</button><button type="button" disabled={!search || filter.collections.includes(search) || filter.collections.length >= 100} onClick={() => toggle(search)}>Add URI</button></form>
      {loading ? <p className="document-hint" role="status">Loading collections…</p> : error ? <p className="document-error" role="alert">{error} You can still add an exact collection URI above.</p> : <div className="collection-options">{page?.collections.map(uri => <label key={uri} title={uri}><input type="checkbox" checked={filter.collections.includes(uri)} disabled={filter.collections.length >= 100 && !filter.collections.includes(uri)} onChange={() => toggle(uri)} /><span>{uri || '(empty collection URI)'}</span></label>)}{!page?.collections.length && <p className="document-hint">No collections found.</p>}</div>}
      {(afters.length > 1 || page?.next) && <div className="document-pagination"><button aria-label="Previous collections" disabled={loading || afters.length === 1} onClick={() => setAfters(old => old.slice(0, -1))}><ChevronLeft size={14} /></button><span>Page {afters.length} · 100 per page</span><button aria-label="Next collections" disabled={loading || !page?.next} onClick={() => setAfters(old => [...old, page!.next])}><ChevronRight size={14} /></button></div>}
    </details>
    <form className="collection-save" onSubmit={event => { event.preventDefault(); if (!name.trim() || !filter.collections.length) return; const next = { ...filter, name: name.trim() }; updateSets([...sets.filter(set => set.name !== next.name), next]); setSaved(next.name); }}><input aria-label="Collection set name" placeholder="Name this collection set…" value={name} onChange={event => setName(event.target.value)} /><button disabled={!name.trim() || !filter.collections.length}>Save set</button></form>
    {storageError && <p className="document-warning" role="alert">{storageError}</p>}
  </section>;
}
