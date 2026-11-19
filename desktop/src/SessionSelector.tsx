import { useEffect, useRef, useState, type FormEvent } from 'react';
import { Check, ChevronDown, FolderKanban, Pencil, Plus, Search, X } from 'lucide-react';
import type { WorkbenchSession } from './workbench-sessions';

export type SessionAction = { kind: 'open'; id: string } | { kind: 'create'; name: string } | { kind: 'rename'; name: string };

export function SessionSelector({ active, sessions, busy, focusOnMount, onAction }: {
  active: WorkbenchSession;
  sessions: WorkbenchSession[];
  busy: boolean;
  focusOnMount: boolean;
  onAction: (action: SessionAction) => Promise<string | undefined>;
}) {
  const trigger = useRef<HTMLButtonElement>(null);
  const dialog = useRef<HTMLDialogElement>(null);
  const searchField = useRef<HTMLInputElement>(null);
  const nameField = useRef<HTMLInputElement>(null);
  const [search, setSearch] = useState('');
  const [mode, setMode] = useState<'create' | 'rename' | null>(null);
  const [name, setName] = useState('');
  const [error, setError] = useState('');
  const [pending, setPending] = useState(false);
  const visible = sessions.filter(session => session.name.toLowerCase().includes(search.trim().toLowerCase()));

  function open() {
    const element = dialog.current;
    if (!element || element.open) return;
    setSearch(''); setMode(null); setError('');
    if (typeof element.showModal === 'function') element.showModal();
    else element.open = true;
    searchField.current?.focus();
  }

  function close() {
    const element = dialog.current;
    if (!element?.open) return;
    if (typeof element.close === 'function') element.close();
    else element.open = false;
    trigger.current?.focus();
  }

  useEffect(() => {
    if (focusOnMount) trigger.current?.focus();
  }, [focusOnMount]);
  useEffect(() => {
    function shortcut(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.shiftKey && event.code === 'KeyK') {
        event.preventDefault();
        open();
      }
    }
    window.addEventListener('keydown', shortcut, true);
    return () => window.removeEventListener('keydown', shortcut, true);
  }, []);
  useEffect(() => {
    if (mode) { nameField.current?.focus(); nameField.current?.select(); }
    else if (dialog.current?.open) searchField.current?.focus();
  }, [mode]);

  async function act(action: SessionAction) {
    if (pending) return;
    setPending(true); setError('');
    try {
      const message = await onAction(action);
      if (message) setError(message);
      else close();
    } finally { setPending(false); }
  }

  function edit(kind: 'create' | 'rename') {
    setName(kind === 'rename' ? active.name : '');
    setMode(kind); setError('');
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    if (mode && name.trim()) void act({ kind: mode, name: name.trim() });
  }

  return <>
    <button ref={trigger} className="session-trigger" type="button" aria-haspopup="dialog"
      aria-keyshortcuts="Meta+Shift+K Control+Shift+K" aria-label={`Work session: ${active.name}`}
      title={`Switch work session · ${active.name} (⌘/Ctrl ⇧ K)`} onClick={open}>
      <FolderKanban size={15} aria-hidden="true"/><span>{active.name}</span><ChevronDown size={12} aria-hidden="true"/>
    </button>
    <dialog ref={dialog} className="session-dialog" aria-labelledby="session-dialog-title" onKeyDown={event => event.stopPropagation()}
      onCancel={event => { event.preventDefault(); if (!pending) close(); }}
      onClick={event => { if (event.target === event.currentTarget && !pending) close(); }}>
      <div className="session-dialog-content">
        <header><h2 id="session-dialog-title">Work sessions</h2><button type="button" aria-label="Close work sessions" disabled={pending} onClick={close}><X size={16}/></button></header>
        {mode ? <form onSubmit={submit} className="session-name-form">
          <label>{mode === 'create' ? 'New session name' : 'Rename current session'}
            <input ref={nameField} value={name} maxLength={80} required placeholder="e.g. OPS-123" disabled={pending}
              onChange={event => { setName(event.target.value); setError(''); }}/>
          </label>
          {mode === 'create' && <p>Starts with the current environment and query folder.</p>}
          <div><button type="button" disabled={pending} onClick={() => { setMode(null); setError(''); }}>Back</button>
            <button type="submit" className="run-button" disabled={pending || !name.trim() || (mode === 'create' && busy)}>{mode === 'create' ? 'Create session' : 'Save name'}</button></div>
        </form> : <>
          <label className="session-search"><Search size={14} aria-hidden="true"/><input ref={searchField} aria-label="Find sessions" placeholder="Find a session…" value={search}
            onChange={event => setSearch(event.target.value)} onKeyDown={event => {
              if (event.key === 'ArrowDown') {
                event.preventDefault();
                dialog.current?.querySelector<HTMLButtonElement>('.session-list button:not(:disabled)')?.focus();
              } else if (event.key === 'Enter' && !busy) {
                const match = search.trim() ? visible[0] : visible.find(item => item.id !== active.id);
                if (match) void act({ kind: 'open', id: match.id });
              }
            }}/></label>
          <ul className="session-list">
            {visible.map(session => <li key={session.id}><button type="button" disabled={pending || (busy && session.id !== active.id)}
              aria-current={session.id === active.id ? 'true' : undefined} onClick={() => void act({ kind: 'open', id: session.id })}>
              <span>{session.name}</span>{session.id === active.id && <Check size={15} aria-hidden="true"/>}
            </button></li>)}
          </ul>
          {!visible.length && <p>No matching sessions.</p>}
          <footer><button type="button" disabled={pending} onClick={() => edit('rename')}><Pencil size={13}/>Rename current</button>
            <button type="button" className="run-button" disabled={pending || busy} onClick={() => edit('create')}><Plus size={14}/>New session</button></footer>
        </>}
        {error && <p className="session-error" role="alert">{error}</p>}
        {busy && <p role="status">Finish the current workspace operation to switch sessions.</p>}
      </div>
    </dialog>
  </>;
}
