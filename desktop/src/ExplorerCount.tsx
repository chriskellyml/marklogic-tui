import { useEffect, useRef, useState } from 'react';

export function ExplorerCount({ count }: { count: number }) {
  const previous = useRef(count);
  const [pulse, setPulse] = useState(0);
  useEffect(() => {
    if (count > previous.current) setPulse(value => value + 1);
    previous.current = count;
  }, [count]);
  return <span key={pulse} aria-hidden="true" className={`explorer-count ${pulse ? 'increased' : ''}`}>{count}</span>;
}
