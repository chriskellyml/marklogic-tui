import { useEffect, useState } from 'react';
import { Search, X } from 'lucide-react';
import { api, errorMessage, type VariableBinding, type QueryVariables } from './api';

export function ExternalVariables({ source, values, onChange, disabled }: {
  source: string; values: Record<string, VariableBinding>; onChange: (values: Record<string, VariableBinding>) => void; disabled: boolean;
}) {
  const [data, setData] = useState<QueryVariables | null>(null);
  const [error, setError] = useState('');
  const [picker, setPicker] = useState<string | null>(null);
  const [search, setSearch] = useState('');
  useEffect(() => {
    let active = true;
    setData(null);
    const timer = setTimeout(() => {
      void api.queryVariables(source).then(result => { if (active) { setData(result); setError(''); } })
        .catch(e => { if (active) setError(errorMessage(e)); });
    }, 200);
    return () => { active = false; clearTimeout(timer); };
  }, [source]);
  useEffect(() => {
    if (!picker) return;
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') setPicker(null); };
    window.addEventListener('keydown', escape);
    return () => window.removeEventListener('keydown', escape);
  }, [picker]);
  if (!data?.variables.length) return error ? <aside className="external-variables"><p role="alert">{error}</p></aside> : null;
  const update = (name: string, change: Partial<VariableBinding>) => onChange({ ...values, [name]: { ...(values[name] ?? { value: '' }), ...change } });
  return <aside className="external-variables" aria-label="External variables">
    <strong>External variables</strong>
    <p>Enter values or choose a global value. Leave unset to use the query default.</p>
    {data.variables.map(variable => {
      const binding = values[variable.name];
      return <div className="external-variable" key={variable.name}>
        <label htmlFor={`external-${variable.name}`}>${variable.name} <code>{variable.sequenceType}</code></label>
        <div className="external-value-row"><input id={`external-${variable.name}`} aria-label={`Value for $${variable.name}`} disabled={disabled}
          inputMode={!binding?.expression && /^xs:(?:int|integer|long|short|byte|decimal|double|float|positiveInteger|nonNegativeInteger)/.test(variable.sequenceType) ? 'decimal' : 'text'}
          list={variable.sequenceType.startsWith('xs:boolean') ? `boolean-${variable.name}` : undefined}
          value={binding?.value ?? ''} placeholder={variable.defaultValue ? `Default: ${variable.defaultValue}` : 'Required value'}
          onChange={event => update(variable.name, { value: event.target.value })} />
          <button disabled={disabled} aria-label={`Choose global for $${variable.name}`} title="Choose global value" onClick={() => { setPicker(variable.name); setSearch(''); }}><Search size={14} /></button></div>
        {variable.sequenceType.startsWith('xs:boolean') && <datalist id={`boolean-${variable.name}`}><option value="true"/><option value="false"/></datalist>}
        <div className="external-variable-options"><label><input type="checkbox" checked={binding?.expression ?? false} disabled={disabled} onChange={event => update(variable.name, { expression: event.target.checked })} />XQuery expression</label>
          {binding && <button disabled={disabled} onClick={() => { const next = { ...values }; delete next[variable.name]; onChange(next); }}>Reset</button>}</div>
        <small>{binding ? 'Value supplied' : variable.defaultValue ? 'Using query default' : 'Unset'}{/[?*+]$/.test(variable.sequenceType) ? ' · Use expression mode for sequences or ()' : ''}</small>
      </div>;
    })}
    {picker && <div className="scratch-save-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) setPicker(null); }}><div className="scratch-save-dialog global-value-picker" role="dialog" aria-modal="true" aria-labelledby="global-picker-title">
      <div className="global-picker-heading"><h2 id="global-picker-title">Global value for ${picker}</h2><button aria-label="Close global values" onClick={() => setPicker(null)}><X size={16}/></button></div>
      <input autoFocus aria-label="Search global values" placeholder="Search names or values" value={search} onChange={event => setSearch(event.target.value)} />
      <div className="global-value-list">{Object.entries(data.globals).filter(([name, value]) => `${name} ${value}`.toLowerCase().includes(search.toLowerCase())).map(([name, value]) => <button key={name} onClick={() => { update(picker, { value: `%%${name}%%` }); setPicker(null); }}><code>%%{name}%%</code><span>{value}</span></button>)}</div>
      {!Object.keys(data.globals).length && <p>No global values configured in active_variables.</p>}
    </div></div>}
  </aside>;
}
