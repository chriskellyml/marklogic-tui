import { useEffect, useRef, useState, type CSSProperties, type PointerEvent } from 'react';
import { GripHorizontal, X } from 'lucide-react';
import { errorMessage } from './api';
import { logsApi } from './logs-api';
import { appendRules, bulkRules, FILTER_FIELDS, FILTER_LISTS_KEY, readFilterLists, type FilterAction, type FilterField, type FilterList, type FilterRule } from './log-filters';

interface Props {
  action: FilterAction; rules: FilterRule[]; onChange: (rules: FilterRule[]) => void;
  onAction: (action: FilterAction) => void; onClose: (cancel?: boolean) => void;
  position: { x: number; y: number } | null; onPosition: (position: { x: number; y: number }) => void;
}
export function LogFilterDialog({ action, rules, onChange, onAction, onClose, position, onPosition }: Props) {
  const dialog = useRef<HTMLElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  const drag = useRef<{ x: number; y: number } | null>(null);
  const [draft, setDraft] = useState('');
  const [field, setField] = useState<FilterField>('any');
  const [mode, setMode] = useState<FilterRule['mode']>('literal');
  const [matchCase, setMatchCase] = useState<FilterRule['case']>('insensitive');
  const [validation, setValidation] = useState({ pending: true, error: '' });
  const [lists, setLists] = useState<FilterList[]>([]);
  const [storageError, setStorageError] = useState('');
  const [selected, setSelected] = useState('');
  const [name, setName] = useState('');
  const [notice, setNotice] = useState('');
  useEffect(() => {
    try { setLists(readFilterLists()); } catch (error) { setStorageError(errorMessage(error)); }
  }, []);
  useEffect(() => { input.current?.focus(); }, [action]);
  useEffect(() => {
    let alive = true;
    setValidation({ pending: true, error: '' });
    const timer = setTimeout(() => {
      void logsApi.validateFilterRules([...rules, ...bulkRules(draft, action, { field, mode, case: matchCase })])
        .then(() => { if (alive) setValidation({ pending: false, error: '' }); })
        .catch((error) => { if (alive) setValidation({ pending: false, error: errorMessage(error) }); });
    }, 120);
    return () => { alive = false; clearTimeout(timer); };
  }, [rules, draft, action, field, mode, matchCase]);
  // Keep the floating window reachable after resizing or changing the files pane.
  useEffect(() => {
    const fit = () => {
      const rect = dialog.current?.getBoundingClientRect();
      if (rect) onPosition({ x: Math.max(8, Math.min(rect.x, window.innerWidth - rect.width - 8)), y: Math.max(8, Math.min(rect.y, window.innerHeight - rect.height - 8)) });
    };
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(fit);
    if (dialog.current) observer?.observe(dialog.current);
    fit();
    window.addEventListener('resize', fit);
    return () => { observer?.disconnect(); window.removeEventListener('resize', fit); };
  }, [onPosition]);
  const shown = rules.filter((rule) => rule.action === action);
  const invalid = validation.pending || Boolean(validation.error);
  function add() {
    const next = appendRules(rules, bulkRules(draft, action, { field, mode, case: matchCase }));
    onChange(next); setDraft(''); input.current?.focus();
    return next;
  }
  function update(id: string, patch: Partial<FilterRule>) { onChange(rules.map((rule) => rule.id === id ? { ...rule, ...patch } : rule)); }
  function persist(next: FilterList[], message: string) {
    try {
      window.localStorage.setItem(FILTER_LISTS_KEY, JSON.stringify({ version: 1, lists: next }));
      setLists(next); setStorageError(''); setNotice(message); return true;
    } catch (error) { setStorageError(`Could not save filter lists: ${errorMessage(error)}`); return false; }
  }
  function save() {
    const list = { id: crypto.randomUUID(), name: name.trim(), rules: structuredClone(rules) };
    if (persist([...lists, list], `Saved “${list.name}”.`)) setSelected(list.id);
  }
  const list = lists.find((item) => item.id === selected);
  function startDrag(event: PointerEvent<HTMLElement>) {
    if ((event.target as HTMLElement).closest('button') || event.button !== 0) return;
    event.preventDefault();
    const rect = dialog.current!.getBoundingClientRect();
    drag.current = { x: event.clientX - rect.x, y: event.clientY - rect.y };
    event.currentTarget.setPointerCapture(event.pointerId);
  }
  const style: CSSProperties = position ? { left: position.x, top: position.y } : {};
  return <section ref={dialog} className="log-filter-editor" role="dialog" aria-modal="false" aria-label={action === 'include' ? 'Filter in' : 'Filter out'} style={style}
    onKeyDown={(event) => {
      event.stopPropagation();
      if (event.key === 'Escape') { event.preventDefault(); onClose(true); }
      else if ((event.ctrlKey || event.metaKey) && event.key === 'Enter' && !invalid) { event.preventDefault(); if (draft.trim()) add(); onClose(); }
    }}>
    <header className="filter-drag-handle" tabIndex={0} aria-label="Move filter dialog with arrow keys" onKeyDown={(event) => {
      if (event.target !== event.currentTarget || !['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) return;
      event.preventDefault();
      const rect = dialog.current!.getBoundingClientRect();
      const step = event.shiftKey ? 40 : 10;
      onPosition({ x: Math.max(8, Math.min(rect.x + (event.key === 'ArrowLeft' ? -step : event.key === 'ArrowRight' ? step : 0), window.innerWidth - rect.width - 8)), y: Math.max(8, Math.min(rect.y + (event.key === 'ArrowUp' ? -step : event.key === 'ArrowDown' ? step : 0), window.innerHeight - rect.height - 8)) });
    }} onPointerDown={startDrag} onPointerMove={(event) => {
      if (!drag.current) return;
      const rect = dialog.current!.getBoundingClientRect();
      onPosition({ x: Math.max(8, Math.min(event.clientX - drag.current.x, window.innerWidth - rect.width - 8)), y: Math.max(8, Math.min(event.clientY - drag.current.y, window.innerHeight - rect.height - 8)) });
    }} onPointerUp={() => { drag.current = null; }} onPointerCancel={() => { drag.current = null; }}>
      <GripHorizontal size={16} aria-hidden="true" /><strong>Content filters</strong><span>Drag to move · 75% transparent away</span><button aria-label="Cancel filter changes" onClick={() => onClose(true)}><X size={14} /></button>
    </header>
    <div className="filter-tabs"><button aria-pressed={action === 'include'} onClick={() => onAction('include')}>Keep / filter in ({rules.filter((r) => r.action === 'include' && r.enabled).length})</button><button aria-pressed={action === 'exclude'} onClick={() => onAction('exclude')}>Drop / filter out ({rules.filter((r) => r.action === 'exclude' && r.enabled).length})</button></div>
    <p>Keep any matching enabled term; drop any matching enabled term. Drop wins. Fields refer to whole records; missing fields never match.</p>
    <div className="filter-composer-options">
      <label>Field<select aria-label="New term field" value={field} onChange={(e) => setField(e.target.value as FilterField)}>{Object.entries(FILTER_FIELDS).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
      <label>Match<select aria-label="New term match" value={mode} onChange={(e) => setMode(e.target.value as FilterRule['mode'])}><option value="literal">Contains text</option><option value="exact">Exact value</option><option value="regex">Regex</option></select></label>
      <label>Case<select aria-label="New term case" value={matchCase} onChange={(e) => setMatchCase(e.target.value as FilterRule['case'])}><option value="insensitive">Ignore case</option><option value="sensitive">Match case</option><option value="smart">Smart case</option></select></label>
    </div>
    <label>Add terms — one per line<textarea ref={input} aria-label={action === 'include' ? 'Filter in text' : 'Filter out text'} value={draft} autoCapitalize="off" autoCorrect="off" spellCheck={false} rows={3} placeholder="Type or paste terms, one per line" onChange={(e) => setDraft(e.target.value)} onKeyDown={(e) => { if (e.ctrlKey && e.key.toLowerCase() === 'u') { e.preventDefault(); setDraft(''); } }} /></label>
    <div className="filter-bulk-actions"><button className="log-tool" disabled={!draft.trim() || invalid} onClick={add}>Add terms</button><span>Commas and punctuation stay in each term.</span></div>
    {validation.error && <p className="filter-error" role="alert">{validation.error}</p>}
    <div className="filter-bulk-actions"><strong>{shown.length} terms · {shown.filter((r) => r.enabled).length} enabled</strong>
      <button className="log-tool" disabled={!shown.length} onClick={() => onChange(rules.map((r) => r.action === action ? { ...r, enabled: true } : r))}>Enable all</button>
      <button className="log-tool" disabled={!shown.length} onClick={() => onChange(rules.map((r) => r.action === action ? { ...r, enabled: false } : r))}>Disable all</button>
      <button className="log-tool" disabled={!shown.length} onClick={() => onChange(rules.filter((r) => r.action !== action))}>Remove all</button></div>
    <div className="filter-rule-list">{shown.map((rule, index) => <div className={`filter-rule ${rule.enabled ? '' : 'disabled'}`} key={rule.id}>
      <input type="checkbox" aria-label={`Enable term ${index + 1}`} checked={rule.enabled} onChange={(e) => update(rule.id, { enabled: e.target.checked })} />
      <input aria-label={`Term ${index + 1}`} value={rule.text} autoCapitalize="off" autoCorrect="off" spellCheck={false} onChange={(e) => update(rule.id, { text: e.target.value })} />
      <select aria-label={`Field for term ${index + 1}`} value={rule.field} onChange={(e) => update(rule.id, { field: e.target.value as FilterField })}>{Object.entries(FILTER_FIELDS).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
      <select aria-label={`Match for term ${index + 1}`} value={rule.mode} onChange={(e) => update(rule.id, { mode: e.target.value as FilterRule['mode'] })}><option value="literal">Contains</option><option value="exact">Exact</option><option value="regex">Regex</option></select>
      <select aria-label={`Case for term ${index + 1}`} value={rule.case} onChange={(e) => update(rule.id, { case: e.target.value as FilterRule['case'] })}><option value="insensitive">Ignore case</option><option value="sensitive">Match case</option><option value="smart">Smart case</option></select>
      <button aria-label={`Remove term ${index + 1}`} onClick={() => onChange(rules.filter((r) => r.id !== rule.id))}><X size={13} /></button>
    </div>)}{!shown.length && <p>No {action === 'include' ? 'keep' : 'drop'} terms yet.</p>}</div>
    <fieldset className="filter-saved-lists"><legend>Saved lists · includes both keep and drop rules</legend>
      <div><select aria-label="Saved filter lists" value={selected} onChange={(e) => { setSelected(e.target.value); setName(lists.find((l) => l.id === e.target.value)?.name ?? ''); setNotice(''); }}><option value="">Choose a list</option>{lists.map((item) => <option key={item.id} value={item.id}>{item.name}</option>)}</select>
        <button className="log-tool" disabled={!list} onClick={() => { onChange(structuredClone(list!.rules)); setNotice(`Loaded “${list!.name}”.`); }}>Load</button>
        <button className="log-tool" disabled={!list} onClick={() => onChange(appendRules(rules, list!.rules))}>Append</button>
        <button className="log-tool" disabled={!list} onClick={() => { if (persist(lists.filter((l) => l.id !== selected), `Deleted “${list!.name}”.`)) { setSelected(''); setName(''); } }}>Delete list</button></div>
      <div><input aria-label="Filter list name" placeholder="List name" value={name} autoCapitalize="off" autoCorrect="off" spellCheck={false} onChange={(e) => setName(e.target.value)} />
        <button className="log-tool" disabled={!name.trim() || !rules.length || invalid || Boolean(draft.trim())} onClick={save}>Save as new</button>
        <button className="log-tool" disabled={!list || !name.trim() || invalid || Boolean(draft.trim())} onClick={() => persist(lists.map((l) => l.id === selected ? { ...l, name: name.trim(), rules: structuredClone(rules) } : l), 'List updated.')}>Update list</button>
        <button className="log-tool" disabled={!list || !name.trim()} onClick={() => persist(lists.map((l) => l.id === selected ? { ...l, name: name.trim() } : l), 'List renamed.')}>Rename</button></div>
      {draft.trim() && <p>Add pending terms before saving a list.</p>}
      {storageError && <p className="filter-error" role="alert">{storageError}</p>}{notice && <p role="status">{notice}</p>}
    </fieldset>
    <footer><span>Changes preview live. Esc restores the opening rules; saved lists are kept.</span><button className="log-tool" disabled={invalid} onClick={() => { if (draft.trim()) add(); onClose(); }}>Apply</button><button className="log-tool" onClick={() => onClose(true)}>Cancel</button></footer>
  </section>;
}
