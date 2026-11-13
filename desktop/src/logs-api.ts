import { Channel, invoke } from '@tauri-apps/api/core';
import type { Connection } from './api';
import type { FilterRule } from './log-filters';

export interface LogFile {
  id: string; host: string; name: string; kind: string; port: string; size: number;
  cachedBytes: number; cached: boolean; complete: boolean; stale: boolean; remote: boolean;
  days?: string[]; modified: string; updatedAt: string | null;
}
export interface LogView {
  session: number; files: LogFile[]; hosts: string[]; now: string; cacheRoot: string;
  warnings: string[]; errors: Record<string, string>;
}
export interface LogProgress {
  phase: string; current: string; completed: number; total: number; bytes: number; message: string;
}
export interface LogQuery {
  requestId?: number;
  merged?: boolean; splitHosts?: boolean;
  cells?: { source: string; day: string }[];
  files: string[]; hosts: string[]; search: string; include: string; exclude: string;
  filterRules?: FilterRule[];
  errorsOnly: boolean; start: string | null; end: string | null; offset: number; newest: boolean;
  raw: boolean; fullErrors: boolean; contextBefore: number; contextAfter: number;
}
export interface LogRow {
  id: string; ordinal?: number; mergedOrdinal?: number | null; hostOrdinal?: number | null; source: string; host: string; file: string; time: string | null;
  level: string; kind: string; port: string; method?: string | null; text: string; lines?: { text: string; color: string | null }[][] | null; raw: string; matched: boolean;
}
export interface LogPage {
  rows: LogRow[]; total: number; offset: number; hosts: { host: string; count: number }[];
  pageSize?: number; pageTotal?: number; hostTimeline?: Record<string, { time: string; count: number }[]>;
  coverage?: Record<string, string[]>;
  timeline: { time: string; count: number }[]; first: string | null; last: string | null;
  availableFirst?: string | null; availableLast?: string | null;
  availableHosts?: Record<string, { first: string | null; last: string | null }>; warnings: string[];
}
function channel(onProgress: (value: LogProgress) => void) {
  const events = new Channel<LogProgress>(); events.onmessage = onProgress; return events;
}
export const logsApi = {
  validateFilterRules: (rules: FilterRule[]) => invoke<void>('logs_validate_filter_rules', { rules }),
  health: (connection: Connection) => invoke<string>('logs_health', { connection }),
  open: (connection: Connection, directory: string) => invoke<LogView>('logs_open', { connection, directory }),
  discover: (session: number, onProgress: (value: LogProgress) => void) =>
    invoke<LogView>('logs_discover', { session, events: channel(onProgress) }),
  download: (session: number, files: string[], full: boolean, onProgress: (value: LogProgress) => void, automatic = false) =>
    invoke<LogView>('logs_download', { session, files, full, automatic, events: channel(onProgress) }),
  page: (session: number, query: LogQuery, onProgress: (value: LogProgress) => void) =>
    invoke<LogPage>('logs_page', { session, query, events: channel(onProgress) }),
  cancelSearch: (session: number, requestId: number) => invoke<void>('logs_cancel_search', { session, requestId }),
  cancel: (session: number) => invoke<void>('logs_cancel', { session }),
  close: (session: number) => invoke<void>('logs_close', { session }),
};
