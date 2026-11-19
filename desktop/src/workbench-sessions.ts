import { createContext, useContext, useLayoutEffect, useRef, useState } from 'react';

export interface WorkbenchSession {
  id: string;
  name: string;
}

export interface SessionIndex {
  activeId: string;
  sessions: WorkbenchSession[];
}

export const defaultSessionId = 'default';
const indexKey = 'workbench.sessions.v1';

export function readSessionIndex(): SessionIndex {
  try {
    const value = JSON.parse(localStorage.getItem(indexKey) ?? 'null') as SessionIndex | null;
    if (value && Array.isArray(value.sessions)) {
      const sessions = value.sessions.filter(item => item && typeof item.name === 'string' && item.name.trim() &&
        typeof item.id === 'string' && /^[a-z0-9-]+$/.test(item.id));
      if (sessions.some(item => item.id === value.activeId)) return { activeId: value.activeId, sessions };
    }
  } catch { /* Use the original workspace if the index is unavailable. */ }
  return { activeId: defaultSessionId, sessions: [{ id: defaultSessionId, name: 'Default' }] };
}

export function saveSessionIndex(index: SessionIndex) {
  localStorage.setItem(indexKey, JSON.stringify(index));
}

// The original workspace uses its existing keys so upgrades retain saved data in place.
export function sessionKey(id: string, key: string) {
  return id === defaultSessionId ? key : `workbench.session.v1:${id}:${key}`;
}

export function readStored<T>(key: string, fallback: T): T {
  try {
    const value = localStorage.getItem(key);
    return value === null ? fallback : JSON.parse(value) as T;
  } catch { return fallback; }
}

interface SessionContextValue {
  session: WorkbenchSession;
  reportStorage: (key: string, failed: boolean) => void;
}

export const SessionContext = createContext<SessionContextValue | null>(null);
export const useWorkbenchSession = () => useContext(SessionContext);
export function useSessionKey(key: string) {
  return sessionKey(useWorkbenchSession()?.session.id ?? defaultSessionId, key);
}

export function useSessionState<T>(name: string, initial: T | (() => T)) {
  const context = useWorkbenchSession();
  const key = sessionKey(context?.session.id ?? defaultSessionId, `workbench.session-view.v1:${name}`);
  const report = context?.reportStorage;
  const [value, setValue] = useState<T>(() => {
    if (context) {
      try {
        const stored = localStorage.getItem(key);
        if (stored !== null) return JSON.parse(stored) as T;
      } catch { /* Use the screen default. */ }
    }
    return typeof initial === 'function' ? (initial as () => T)() : initial;
  });
  const persisted = useRef(value);
  useLayoutEffect(() => {
    if (!report || Object.is(persisted.current, value)) return;
    try {
      const serialized = JSON.stringify(value);
      if (localStorage.getItem(key) !== serialized) localStorage.setItem(key, serialized);
      persisted.current = value;
      report(key, false);
    } catch { report(key, true); }
  }, [key, report, value]);
  return [value, setValue] as const;
}

export const queryRootKey = 'workbench.query-root.v1';
