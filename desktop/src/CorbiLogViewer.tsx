import { useEffect, useMemo, useRef, useState } from 'react';
import { corbiLogRecords, logTone } from './corbi-output';

export function CorbiLogViewer({ content }: { content: string }) {
  const [search, setSearch] = useState('');
  const [follow, setFollow] = useState(true);
  const scroll = useRef<HTMLDivElement>(null);
  const records = useMemo(() => corbiLogRecords(content), [content]);
  const visible = useMemo(() => records.filter(record => (record.time + ' ' + record.level + ' ' + record.message).toLowerCase().includes(search.toLowerCase())), [records, search]);
  useEffect(() => {
    if (follow && scroll.current) scroll.current.scrollTop = scroll.current.scrollHeight;
  }, [content, search, follow]);
  useEffect(() => {
    const element = scroll.current;
    if (!element || !follow || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => { element.scrollTop = element.scrollHeight; });
    observer.observe(element);
    return () => observer.disconnect();
  }, [follow]);
  return <>
    <div className="corbi-view-controls"><input aria-label="Filter run log" placeholder="Filter log…" value={search} onChange={e => setSearch(e.target.value)}/>
      <label><input type="checkbox" checked={follow} onChange={e => setFollow(e.target.checked)}/>Follow tail</label></div>
    <div ref={scroll} className="corbi-log-scroll" aria-label="Run log" tabIndex={0}>
      {visible.map((record, index) => <div className="corbi-log-record" data-tone={logTone(record)} key={index}>
        <span className="corbi-log-time">{record.time}</span><span className="corbi-log-level">{record.level}</span><span className="corbi-log-message">{record.message}</span>
      </div>)}
      {!visible.length && <p className="corbi-note">{content ? 'No matching log entries.' : 'Waiting for log output…'}</p>}
    </div>
  </>;
}
