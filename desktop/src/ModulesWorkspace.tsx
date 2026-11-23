import { useEffect, useRef, useState } from 'react';
import { useSessionState } from './workbench-sessions';
import { Download, RefreshCw, ChevronLeft, ChevronRight, X } from 'lucide-react';
import { errorMessage, type Connection } from './api';
import { modulesApi, moduleLanguage, type CachedModule, type ModuleContext } from './modules-api';
import { QueryTree } from './QueryTree';

export function ModuleBrowser({ connection, database, selectedUri, selectedPending, revision, disabled, onOpen, favoriteUris = [], onFavorite, onBusy, onError }: {
  connection:Connection; database:string; selectedUri?:string; selectedPending:boolean; revision:number; disabled:boolean;
  onOpen:(context:ModuleContext, content:{content:string;original:string}, pending:boolean)=>Promise<void>;
  favoriteUris?:string[]; onFavorite?:(uri:string)=>void;
  onBusy:(busy:boolean)=>void; onError:(error:string)=>void;
}) {
  const [remote,setRemote]=useState<string[]>([]), [total,setTotal]=useState(0), [offset,setOffset]=useState(0);
  const remoteFiles = remote.filter((uri) => !uri.endsWith('/'));
  const [cache,setCache]=useState<CachedModule[]>([]), [selected,setSelected]=useState<Set<string>>(new Set());
  const [busy,setBusy]=useState('');
  const [moduleSearch,setModuleSearch]=useSessionState('query.modules.filter', '');
  const [searched,setSearched]=useState('');
  const [showRemote,setShowRemote]=useState(false);
  const generation=useRef(0);
  const browser=useRef<HTMLElement>(null);
  const key=JSON.stringify([connection.server,connection.appServer,database]);
  useEffect(()=>{generation.current++;setRemote([]);setTotal(0);setSelected(new Set());setOffset(0);},[key]);
  useEffect(()=>{
    let alive=true;setCache([]);
    if(database&&connection.server) void modulesApi.cache(connection,database).then((list)=>{if(alive)setCache(list);}).catch((e)=>{if(alive)onError(errorMessage(e));});
    return()=>{alive=false;};
  },[key,revision]);
  useEffect(()=>{onBusy(Boolean(busy));},[busy,onBusy]);
  useEffect(()=>{
    function outside(event:PointerEvent){if(showRemote&&browser.current&&!browser.current.contains(event.target as Node))setShowRemote(false);}
    function escape(event:KeyboardEvent){if(event.key==='Escape'&&showRemote){event.preventDefault();setShowRemote(false);}}
    document.addEventListener('pointerdown',outside);document.addEventListener('keydown',escape);
    return()=>{document.removeEventListener('pointerdown',outside);document.removeEventListener('keydown',escape);};
  },[showRemote]);
  async function task(label:string,work:()=>Promise<void>){if(busy||disabled)return;setBusy(label);try{await work();}catch(e){onError(errorMessage(e));}finally{setBusy('');}}
  async function refresh(){setCache(await modulesApi.cache(connection,database));}
  async function search(nextOffset=0){setShowRemote(true);const query=nextOffset===0?moduleSearch:searched;const current=generation.current;await task('Searching…',async()=>{const result=await modulesApi.search(connection,database,query,nextOffset);if(current!==generation.current)return;setRemote(result.uris);setTotal(result.total);setOffset(nextOffset);setSearched(query);setSelected(new Set());});}
  return <section ref={browser} className="module-browser" aria-label="Modules of interest">
    <div className="source-heading"><strong>MODULES</strong><span>{cache.filter((m)=>m.unsynced).length?`${cache.filter((m)=>m.unsynced).length} unsynced`:database}</span><button aria-label="Refresh modules" title="Refresh downloaded modules" disabled={disabled||Boolean(busy)||!database} onClick={()=>void task('Refreshing…',refresh)}><RefreshCw size={13}/></button><button aria-label="Find server modules" title="Search server module URIs" disabled={disabled||Boolean(busy)||!database} onClick={()=>void search()}><Download size={13}/> Download modules</button></div>
    <form className="module-uri-search" onSubmit={(event)=>{event.preventDefault();void search();}}><label><span>Search modules</span><input aria-label="Search modules" placeholder="Module URI…" autoCapitalize="none" autoComplete="off" autoCorrect="off" spellCheck={false} value={moduleSearch} onChange={(event)=>setModuleSearch(event.target.value)} /></label><button disabled={disabled||Boolean(busy)||!database} type="submit">Search</button></form>
    {showRemote&&<div className="remote-module-picker"><div className="source-heading"><label><input type="checkbox" aria-label="Select all module results" checked={remoteFiles.length>0&&remoteFiles.every((uri)=>selected.has(uri))} onChange={(e)=>setSelected(e.target.checked?new Set(remoteFiles):new Set())}/> {total} matches</label><button aria-label="Download selected modules" title="Download selected modules; keep local edits" disabled={!selected.size||disabled||Boolean(busy)} onClick={()=>void task('Downloading…',async()=>{const failures:string[]=[];for(const uri of selected){try{await modulesApi.checkout({connection,database,uri});}catch(e){failures.push(`${uri}: ${errorMessage(e)}`);}}await refresh();if(failures.length)throw new Error(failures.join('\n'));})}><Download size={14}/></button><button aria-label="Previous module results" disabled={!offset||disabled} onClick={()=>void search(Math.max(0,offset-100))}><ChevronLeft size={12}/></button><button aria-label="Next module results" disabled={offset+100>=total||disabled} onClick={()=>void search(offset+100)}><ChevronRight size={12}/></button><button aria-label="Close module search results" title="Close results" onClick={()=>setShowRemote(false)}><X size={14}/></button></div>{remoteFiles.map((uri)=><label key={uri} className="remote-module-row"><input type="checkbox" checked={selected.has(uri)} onChange={(e)=>setSelected((old)=>{const next=new Set(old);if(e.target.checked)next.add(uri);else next.delete(uri);return next;})}/><span title={uri}>{uri}</span><button aria-label={`Download ${uri}`} title="Download and open" disabled={disabled||Boolean(busy)} onClick={()=>void task('Downloading…',async()=>{const context={connection,database,uri};await modulesApi.checkout(context);await refresh();const content=await modulesApi.open(context);await onOpen(context,content,content.content!==content.original);})}><Download size={12}/></button></label>)}</div>}
    {showRemote&&!busy&&!remote.length&&<p className="explorer-hint">No server modules matched.</p>}
    <QueryTree stateKey="query.modules.tree" items={cache.filter((m)=>!m.uri.endsWith('/')).map((m)=>({path:m.uri,language:moduleLanguage(m.uri),status:m.unsynced||(m.uri===selectedUri&&selectedPending)?'UNSYNCED':undefined}))} filter={moduleSearch} selected={selectedUri} disabled={disabled||Boolean(busy)} focusAction="Favorite" focusedPaths={favoriteUris} onFocus={path=>onFavorite?.(path)} onOpen={(item)=>void task('Opening…',async()=>{const context={connection:{...connection},database,uri:item.path};const content=await modulesApi.open(context);await onOpen(context,content,Boolean(cache.find((m)=>m.uri===item.path)?.unsynced));})}/>
    {!cache.length&&!remote.length&&<p className="explorer-hint">Use Download modules to find and download server modules.</p>}
    <span className="explorer-hint" role="status">{busy}</span>
  </section>;
}
