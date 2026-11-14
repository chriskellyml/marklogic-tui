import type { Connection } from './api';
import { invoke } from '@tauri-apps/api/core';
import { confirm, open } from '@tauri-apps/plugin-dialog';

export interface CorbiProject { name: string; jobs: string[]; scripts: string[] }
export interface CorbiCatalog {
  root: string;
  projects: CorbiProject[];
  permissions: Record<string, Record<string, Record<string, boolean>>>;
}
export interface RunEntry { environment: string; id: string }
export interface CorbiRun {
  connection: Connection;
  modulesDatabase: string;
  id: string;
  project: string;
  job: string;
  historyEnv: string;
  wet: boolean;
  status: 'running' | 'completed' | 'failed' | 'stopped';
}
export interface RunRequest {
  project: string;
  job: string;
  connection: Connection;
  modulesDatabase: string;
  limit: number | null;
  dryRunId: string | null;
}

export function targetLabel(connection: Connection) {
  return [connection.server, connection.appServer, connection.database].filter(Boolean).join(' · ');
}

export const corbiApi = {
  choose: () => open({ directory: true, multiple: false, title: 'Open CoRBi data folder' }),
  open: (path: string | null) => invoke<CorbiCatalog | null>('corbi_open', { path }),
  createJob: (root: string, project: string, name: string) => invoke<CorbiCatalog>('corbi_create_job', { root, project, name }),
  source: (root: string, project: string, file: string) => invoke<string>('corbi_source', { root, project, file }),
  save: (root: string, project: string, file: string, contents: string, expected: string) =>
    invoke<void>('corbi_save', { root, project, file, contents, expected }),
  rename: (root: string, project: string, path: string, name: string) =>
    invoke<CorbiCatalog>('corbi_rename', { root, project, path, name }),
  history: (root: string, project: string) => invoke<RunEntry[]>('corbi_history', { root, project }),
  artifacts: (root: string, project: string, run: RunEntry) =>
    invoke<string[]>('corbi_artifacts', { root, project, ...run }),
  output: (root: string, project: string, run: RunEntry, file: string, tail = true) =>
    invoke<string>('corbi_output', { root, project, ...run, file, tail }),
  start: (root: string, request: RunRequest) => invoke<CorbiRun>('corbi_run', { root, request }),
  status: (root: string) => invoke<CorbiRun | null>('corbi_status', { root }),
  stop: (root: string, id: string) => invoke<void>('corbi_stop', { root, id }),
  confirmWet: (run: CorbiRun) => confirm(
    `Execute ${run.project} / ${run.job} against ${targetLabel(run.connection)} (modules: ${run.modulesDatabase}) using dry run ${run.id}? The dry-run URI limit does not apply.`,
    { title: 'Execute wet run', kind: 'warning', okLabel: 'Execute wet run', cancelLabel: 'Cancel' }),
};
