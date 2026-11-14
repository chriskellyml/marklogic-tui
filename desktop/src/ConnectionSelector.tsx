import { useEffect, useRef, useState, type ReactNode } from 'react';
import type { ServerProfile } from './api';
import type { Environment } from './recent-environments';

/** Stage connection changes until the user applies the complete selection. */
export function ConnectionSelector({ servers, connection, disabled, onApply, children }: {
  servers: ServerProfile[]; connection: Environment; disabled: boolean;
  onApply: (next: Environment) => void;
  children?: (draft: Environment, setDraft: (next: Environment) => void, changed: boolean) => ReactNode;
}) {
  const menu = useRef<HTMLDetailsElement>(null);
  const [draft, setDraft] = useState(connection);
  useEffect(() => {
    function outside(event: PointerEvent) { if (menu.current?.open && !menu.current.contains(event.target as Node)) menu.current.open = false; }
    function escape(event: KeyboardEvent) { if (event.key === 'Escape' && menu.current?.open) { event.preventDefault(); event.stopPropagation(); menu.current.open = false; menu.current.querySelector('summary')?.focus(); } }
    document.addEventListener('pointerdown', outside); document.addEventListener('keydown', escape);
    return () => { document.removeEventListener('pointerdown', outside); document.removeEventListener('keydown', escape); };
  }, []);
  const server = servers.find(item => item.name === draft.server);
  const changed = draft.server !== connection.server || draft.appServer !== connection.appServer || draft.database !== connection.database || draft.modulesDatabase !== connection.modulesDatabase;
  function selectApp(name: string, serverName = draft.server) {
    const endpoint = servers.find(item => item.name === serverName)?.appServers.find(item => item.name === name);
    setDraft({ server: serverName, appServer: name || null, database: endpoint?.content_database ?? 'Documents', modulesDatabase: endpoint?.modules_database ?? 'Modules' });
  }
  function close() { if (menu.current) { menu.current.open = false; menu.current.querySelector('summary')?.focus(); } }
  return <details ref={menu} className="log-connection-menu shared-connection-selector"><summary title="Choose connection" onClick={() => { if (!menu.current?.open) setDraft(connection); }}>{connection.server || 'Choose server'}{connection.appServer ? ` · ${connection.appServer}` : ''}<span>⌄</span></summary>
    <section className="connection-popover" aria-label="Connection settings">
      <label>Server<select aria-label="Server" disabled={disabled} value={draft.server} onChange={event => selectApp('', event.target.value)}>{!servers.length && <option value="">No saved servers</option>}{servers.map(item => <option key={item.name}>{item.name}</option>)}</select></label>
      <label>App server<select aria-label="App server" disabled={disabled} value={draft.appServer ?? ''} onChange={event => selectApp(event.target.value)}><option value="">Profile default</option>{server?.appServers.map(item => <option key={item.name} value={item.name}>{item.name} :{item.port}</option>)}</select></label>
      {children?.(draft, setDraft, changed)}
      <div className="connection-actions"><button onClick={close}>Cancel</button><button disabled={disabled || !draft.server || !draft.database.trim()} onClick={() => { onApply(draft); close(); }}>OK</button></div>
    </section>
  </details>;
}
