import { useEffect, useRef, type ReactNode } from 'react';
import type { Connection, ServerProfile } from './api';

/** The same connection picker is used by every workspace. */
export function ConnectionSelector({ servers, connection, disabled, onServer, onAppServer, children }: {
  servers:ServerProfile[]; connection:Connection; disabled:boolean;
  onServer:(name:string)=>void; onAppServer:(name:string)=>void; children?:ReactNode;
}) {
  const menu=useRef<HTMLDetailsElement>(null);
  useEffect(()=>{
    function outside(event:PointerEvent){if(menu.current?.open && !menu.current.contains(event.target as Node)) menu.current.open=false;}
    function escape(event:KeyboardEvent){if(event.key==='Escape' && menu.current?.open){event.preventDefault();event.stopPropagation();menu.current.open=false;menu.current.querySelector('summary')?.focus();}}
    document.addEventListener('pointerdown',outside);
    document.addEventListener('keydown',escape);
    return()=>{document.removeEventListener('pointerdown',outside);document.removeEventListener('keydown',escape);};
  },[]);
  const server=servers.find((item)=>item.name===connection.server);
  return <details ref={menu} className="log-connection-menu shared-connection-selector"><summary title="Choose connection">{connection.server||'Choose server'}{connection.appServer?` · ${connection.appServer}`:''}<span>⌄</span></summary>
    <section className="connection-popover" aria-label="Connection settings">
      <label>Server<select aria-label="Server" disabled={disabled} value={connection.server} onChange={(e)=>onServer(e.target.value)}>{!servers.length&&<option value="">No saved servers</option>}{servers.map((item)=><option key={item.name}>{item.name}</option>)}</select></label>
      <label>App server<select aria-label="App server" disabled={disabled} value={connection.appServer??''} onChange={(e)=>onAppServer(e.target.value)}><option value="">Profile default</option>{server?.appServers.map((item)=><option key={item.name} value={item.name}>{item.name} :{item.port}</option>)}</select></label>
      {children}
    </section>
  </details>;
}
