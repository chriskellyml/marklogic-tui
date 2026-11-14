import { useMemo, useState } from 'react';
import { reportDelimiter, reportPreview, reportRows } from './corbi-output';

const PAGE_SIZE = 200;

export function CorbiReportViewer({ content, waiting }: { content: string; waiting: boolean }) {
  const [view, setView] = useState<'auto' | 'text' | 'table'>('auto');
  const [delimiter, setDelimiter] = useState('');
  const [header, setHeader] = useState(false);
  const [hidden, setHidden] = useState<number[]>([]);
  const [search, setSearch] = useState('');
  const [page, setPage] = useState(0);
  const preview = useMemo(() => reportPreview(content), [content]);
  const rows = useMemo(() => reportRows(preview.text, delimiter || reportDelimiter(preview.text)), [preview.text, delimiter]);
  const columns = useMemo(() => rows.reduce((max, row) => Math.max(max, row.length), 0), [rows]);
  const mode = view === 'auto' ? columns > 1 ? 'table' : 'text' : view;
  const filtered = useMemo(() => (header ? rows.slice(1) : rows).filter(row => row.some(cell => cell.toLowerCase().includes(search.toLowerCase()))), [rows, header, search]);
  const currentPage = Math.min(page, Math.max(0, Math.ceil(filtered.length / PAGE_SIZE) - 1));
  return <>
    <div className="corbi-view-controls">
      <div role="group" aria-label="Report format"><button aria-pressed={mode === 'text'} onClick={() => setView('text')}>Text</button><button aria-pressed={mode === 'table'} onClick={() => setView('table')}>Table</button></div>
      {mode === 'table' && <>
        <label>Delimiter<select aria-label="Report delimiter" value={delimiter} onChange={e => { setDelimiter(e.target.value); setHidden([]); setPage(0); }}>
          <option value="">Auto</option><option value="|">Pipe</option><option value=",">Comma</option><option value={'\t'}>Tab</option><option value=";">Semicolon</option>
        </select></label>
        <label><input type="checkbox" checked={header} onChange={e => { setHeader(e.target.checked); setPage(0); }}/>First row is header</label>
        <details className="corbi-columns"><summary>Columns</summary><div>
          <button onClick={() => setHidden([])}>Show all</button>
          {Array.from({ length: columns }, (_, i) => <label key={i}><input type="checkbox" checked={!hidden.includes(i)} onChange={e => setHidden(old => e.target.checked ? old.filter(c => c !== i) : [...old, i])}/>{header ? rows[0]?.[i] || `Column ${i + 1}` : `Column ${i + 1}`}</label>)}
        </div></details>
        <input aria-label="Filter report rows" placeholder="Filter rows…" value={search} onChange={e => { setSearch(e.target.value); setPage(0); }}/>
      </>}
    </div>
    {preview.notice && <p className="corbi-note">{preview.notice}</p>}
    {!content ? <p className="corbi-note">{waiting ? 'The report will appear as the job produces results.' : 'This run produced no report.'}</p>
      : mode === 'text' ? <pre className="corbi-report-text" aria-label="Run report" tabIndex={0}>{preview.text}</pre>
      : <><div className="corbi-report-table-scroll" tabIndex={0}><table className="corbi-report-table" aria-label="Run report">
        <thead><tr><th scope="col">#</th>{Array.from({ length: columns }, (_, i) => !hidden.includes(i) && <th scope="col" key={i}>{header ? rows[0]?.[i] || `Column ${i + 1}` : `Column ${i + 1}`}</th>)}</tr></thead>
        <tbody>{filtered.slice(currentPage * PAGE_SIZE, (currentPage + 1) * PAGE_SIZE).map((row, i) => <tr key={i}><td>{currentPage * PAGE_SIZE + i + 1}</td>
          {Array.from({ length: columns }, (_, j) => !hidden.includes(j) && <td key={j}>{row[j] ?? ''}</td>)}</tr>)}</tbody>
      </table></div><div className="corbi-table-pagination"><span>{filtered.length.toLocaleString()} rows</span>
        <button aria-label="Previous report page" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>Previous</button>
        <span>Page {currentPage + 1}</span><button aria-label="Next report page" disabled={(currentPage + 1) * PAGE_SIZE >= filtered.length} onClick={() => setPage(currentPage + 1)}>Next</button></div></>}
  </>;
}
