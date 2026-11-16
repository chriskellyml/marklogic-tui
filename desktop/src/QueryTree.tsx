import { useEffect, useMemo, useRef, useState } from 'react';
import { ChevronDown, ChevronRight, Folder, Pencil, Trash2, Pin } from 'lucide-react';
interface Item { path: string; language?: string; status?: string }
interface Node { path: string; name: string; children: Map<string, Node>; item?: Item }
export function QueryTree({ items, folders = [], filter, selected, disabled, onOpen, onRename, onDelete, onFocus, onCreate }: { items: Item[]; folders?: string[]; filter: string; selected?: string; disabled: boolean; onOpen: (item: Item) => void; onRename?: (item:Item)=>void; onDelete?: (item:Item)=>void; onFocus?: (path:string, kind:'file'|'folder')=>void; onCreate?: (folder:string)=>void }) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [menu,setMenu] = useState<{x:number;y:number;folder:string;item?:Item}|null>(null);
  const menuRef=useRef<HTMLDivElement>(null);
  const trigger=useRef<HTMLElement|null>(null);
  useEffect(()=>{
    if(!menu)return;
    function dismiss(event:PointerEvent){if(!menuRef.current?.contains(event.target as globalThis.Node))setMenu(null);}
    function escape(event:KeyboardEvent){if(event.key==='Escape'){event.preventDefault();event.stopPropagation();setMenu(null);trigger.current?.focus();}}
    document.addEventListener('pointerdown',dismiss);document.addEventListener('keydown',escape,true);
    return()=>{document.removeEventListener('pointerdown',dismiss);document.removeEventListener('keydown',escape,true);};
  },[menu]);
  function context(event:React.MouseEvent, folder:string, item?:Item) {
    if(disabled||(!onCreate&&!onRename&&!onDelete))return;
    event.preventDefault();event.stopPropagation();
    trigger.current=event.currentTarget instanceof HTMLElement?event.currentTarget:null;
    setMenu({x:Math.max(0,Math.min(event.clientX,window.innerWidth-190)),y:Math.max(0,Math.min(event.clientY,window.innerHeight-150)),folder,item});
  }

  const root = useMemo(() => {
    const root: Node = { path:'',name:'',children:new Map() };
    function insert(path: string, item?: Item) {
      let node = root;
      const parts = path.replace(/^\//,'').split('/');
      parts.forEach((name, index) => { const key=parts.slice(0,index+1).join('/'); let child=node.children.get(name); if(!child){child={path:key,name,children:new Map()};node.children.set(name,child);} node=child; });
      node.item=item;
    }
    for(const folder of folders) if(!filter) insert(folder);
    for(const item of items) if(item.path.toLowerCase().includes(filter.toLowerCase())) insert(item.path,item);
    return root;
  },[items,folders,filter]);
  function nodes(node: Node, depth=0): React.ReactNode {
    return [...node.children.values()].sort((a,b)=>Number(Boolean(a.item))-Number(Boolean(b.item)) || a.name.localeCompare(b.name)).map((child)=>child.item ? <div key={child.path} className="file-tree-row" onContextMenu={event=>context(event,child.item!.path.includes('/')?child.item!.path.slice(0,child.item!.path.lastIndexOf('/')):'',child.item)}><button className={`file-button ${selected===child.item.path?'selected':''}`} style={{paddingLeft:10+depth*14}} disabled={disabled} title={child.item.path} aria-current={selected===child.item.path?'page':undefined} onClick={()=>onOpen(child.item!)}><span className={`file-type ${child.item.language}`}>{child.item.language==='javascript'?'JS':child.item.language==='xquery'?'XQ':'·'}</span><span>{child.name}</span>{child.item.status&&<small>{child.item.status}</small>}</button>{onFocus&&<button className="file-row-action" aria-label={`Focus file ${child.item.path}`} title="Focus file" disabled={disabled} onClick={()=>onFocus(child.item!.path,'file')}><Pin size={13}/></button>}{onRename&&<button className="file-row-action" aria-label={`Rename ${child.item.path}`} title="Rename file" disabled={disabled} onClick={()=>onRename(child.item!)}><Pencil size={13}/></button>}{onDelete&&<button className="file-row-action" aria-label={`Delete ${child.item.path}`} title="Delete file" disabled={disabled} onClick={()=>onDelete(child.item!)}><Trash2 size={13}/></button>}</div> : <div key={child.path}><div className="file-tree-row" onContextMenu={event=>context(event,child.path)}><button className="file-button tree-folder" disabled={disabled} style={{paddingLeft:6+depth*14}} aria-label={`Folder ${child.path}`} aria-expanded={Boolean(filter)||!collapsed.has(child.path)} onClick={()=>setCollapsed((old)=>{const next=new Set(old);if(next.has(child.path))next.delete(child.path);else next.add(child.path);return next;})}>{!filter&&collapsed.has(child.path)?<ChevronRight size={12}/>:<ChevronDown size={12}/>}<Folder size={13}/><span>{child.name}</span></button>{onFocus&&<button className="file-row-action" aria-label={`Focus folder ${child.path}`} title="Focus folder" disabled={disabled} onClick={()=>onFocus(child.path,'folder')}><Pin size={13}/></button>}</div>{(filter||!collapsed.has(child.path))&&nodes(child,depth+1)}</div>);
  }
  return <div className="query-tree" aria-label="Query folder hierarchy" onContextMenu={event=>context(event,'')}>
    {nodes(root)}
    {menu&&<div ref={menuRef} className="query-context-menu" role="menu" aria-label="Query actions" style={{left:menu.x,top:menu.y}} onContextMenu={event=>event.preventDefault()}>
      {onCreate&&<button role="menuitem" autoFocus onClick={()=>{onCreate(menu.folder);setMenu(null);}}>New script…</button>}
      {menu.item&&onRename&&<button role="menuitem" onClick={()=>{onRename(menu.item!);setMenu(null);}}>Rename…</button>}
      {menu.item&&onDelete&&<button role="menuitem" onClick={()=>{onDelete(menu.item!);setMenu(null);}}>Delete…</button>}
    </div>}
  </div>;
}
