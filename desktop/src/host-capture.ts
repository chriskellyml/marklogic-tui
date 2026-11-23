import { useEffect, useRef, useState } from 'react';
import { errorMessage, type Connection } from './api';

export interface Capture<T> { capturedAt: string; data: T; cacheWarning?: string }
export interface CaptureError { offline: boolean; message: string }
type Reader<T> = (connection: Connection, cached: boolean, dataset: string) => Promise<Capture<T> | null>;
interface ReadState<T> {
  key: string;
  capture: Capture<T> | null;
  fromCache: boolean;
  busy: boolean;
  error: CaptureError | null;
}

function failure(reason: unknown): CaptureError {
  if (reason && typeof reason === 'object' && 'offline' in reason && 'message' in reason &&
      typeof reason.offline === 'boolean' && typeof reason.message === 'string') {
    return { offline: reason.offline, message: reason.message };
  }
  return { offline: false, message: errorMessage(reason) };
}

export function useHostCapture<T>(active: boolean, connection: Connection, read: Reader<T>, dataset = '', auto = true) {
  const key = JSON.stringify([connection.server, dataset]);
  const empty: ReadState<T> = { key, capture: null, fromCache: false, busy: false, error: null };
  const [state, setState] = useState(empty);
  const sequence = useRef(0);
  const completedRequest = useRef(0);
  const [request, setRequest] = useState<{ key: string; id: number }>();
  const refresh = () => setRequest({ key, id: ++sequence.current });
  const requestId = request?.key === key ? request.id : 0;
  useEffect(() => {
    if (!active || !connection.server) return;
    let current = true;
    let liveLoaded = false;
    const live = auto || requestId > completedRequest.current;
    const update = (change: Partial<ReadState<T>>) => {
      if (current) setState(old => ({ ...(old.key === key ? old : empty), ...change }));
    };
    update(live ? { busy: true, error: null } : { busy: false });
    void read(connection, true, dataset).then(capture => {
      if (capture && current && !liveLoaded) setState(old => {
        if (old.key === key && old.capture && old.capture.capturedAt >= capture.capturedAt) return old;
        return { ...(old.key === key ? old : empty), capture, fromCache: true };
      });
    }).catch(() => { /* A missing or damaged local capture must never block a live read. */ });
    if (live) void read(connection, false, dataset).then(capture => {
      liveLoaded = true;
      update({ capture, fromCache: false });
    }).catch(reason => update({ error: failure(reason) })).finally(() => {
      if (current) completedRequest.current = Math.max(completedRequest.current, requestId);
      update({ busy: false });
    });
    return () => { current = false; };
  }, [active, connection, key, requestId, read, dataset, auto]);
  const { capture, fromCache, busy, error } = state.key === key ? state : empty;
  return { capture, fromCache, busy, error, refresh };
}
