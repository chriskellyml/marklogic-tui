import { Channel, invoke, isTauri } from '@tauri-apps/api/core';
import { confirm, open } from '@tauri-apps/plugin-dialog';
import { getCurrentWindow } from '@tauri-apps/api/window';

export type Language = 'javascript' | 'xquery' | 'text';
export interface AppServer { name: string; port: number; secure: boolean; group?: string; favorite?: boolean; content_database?: string | null; modules_database?: string | null }
export interface ServerSettings { name:string; host:string; port:number; secure:boolean; username:string; authType:'digest'|'basic'|'digestbasic'|'application-level'; insecure:boolean; color?:string|null; stripeColorSecondary?:string|null; appServers:AppServer[]; password?:string }
export interface ServerProfile { name: string; endpoint: string; username?: string; authType?: ServerSettings['authType']; color?: string | null; stripeColorSecondary?: string | null; appServers: AppServer[] }
export interface Workspace {
  servers: ServerProfile[];
  activeServer: string | null;
  activeAppServer: string | null;
  activeDatabase: string | null;
  configPath: string;
  queryRoot: string;
  trackedFolders: string[];
}
export interface Connection { server: string; appServer: string | null; database: string }
export interface QueryFile { path: string; language: Language }
export interface QueryFolder { root: string; files: QueryFile[]; folders?: string[] }
export interface VariableBinding { value: string; expression?: boolean }
export interface QueryVariables { variables: {name: string; sequenceType: string; defaultValue: string | null}[]; globals: Record<string, string> }
export interface QueryResult { items: string[]; elapsedMs: number }
export interface UriResult {uri:string; collections:string[]; permissions:string[]; propertyCount:number|null; error:string|null}
export interface OpenCodeModel { providerId: string; id: string; name: string }

export const api = {
  buildInfo: () => invoke<{build:string;executable:string}>('build_info'),
  updateApp: (onProgress: (message: string) => void) => {
    const events = new Channel<string>(); events.onmessage = onProgress;
    return invoke<void>('update_app', { events });
  },
  restartUpdatedApp: () => invoke<void>('restart_updated_app'),
  loadWorkspace: () => {
    if (!isTauri()) return Promise.reject(new Error('Open the desktop app with npm run tauri dev to access your servers and query files.'));
    return invoke<Workspace>('load_workspace');
  },
  serverSettings: (name:string) => invoke<ServerSettings>('server_settings', { name }),
  saveServerSettings: (originalName:string|null, settings:ServerSettings) => invoke<void>('save_server_settings', { originalName, settings }),
  queryFolder: (root: string) => invoke<QueryFolder>('query_folder', { root }),
  openQuery: (path: string) => invoke<string>('open_query', { path }),
  saveQuery: (path: string, contents: string, expectedContents: string) =>
    invoke<void>('save_query', { path, contents, expectedContents }),
  saveScratchQuery: (name:string, contents:string, language:Language) =>
    invoke<QueryFile>('save_scratch_query', { name, contents, language }),
  renameQuery: (path:string, newPath:string, expectedContents:string|null) => invoke<QueryFile>('rename_query', {path,newPath,expectedContents}),
  deleteQuery: (path:string, expectedContents:string|null) => invoke<void>('delete_query', {path,expectedContents}),
  confirmDeleteQuery: (path:string) => confirm(`Permanently delete ${path}? Any unsaved edits in its editor will also be discarded.`, { title:'Delete query file', kind:'warning', okLabel:'Delete file', cancelLabel:'Cancel' }),
  listDatabases: (connection: Connection) => invoke<string[]>('list_databases', { connection }),
  queryVariables: (source: string) => invoke<QueryVariables>('query_variables', { source }),
  executeQuery: (connection: Connection, source: string, language: Language, path: string | null, bindings?: Record<string, VariableBinding>) =>
    invoke<QueryResult>('execute_query', { connection, source, language, path, bindings }),
  openCodeModels: () => isTauri()
    ? invoke<OpenCodeModel[]>('opencode_models')
    : Promise.reject(new Error('Open MarkLogic Workbench in the desktop app to use OpenCode.')),
  openCodeStartService: () => invoke<void>('opencode_start_service'),
  openCodeReviseQuery: (model: Pick<OpenCodeModel, 'providerId' | 'id'>, prompt: string, source: string, language: Language, history: { request: string; response: string }[] = []) =>
    invoke<string>('opencode_revise_query', { model, prompt, source, language, history }),
  queryUriMetadata: (connection:Connection, uris:string[]) => invoke<UriResult[]>('query_uri_metadata', {connection,uris}),
  chooseFolder: () => open({ directory: true, multiple: false, title: 'Choose a query folder' }),
  discardChanges: () => confirm('Discard unsaved changes?', {
    title: 'Unsaved changes', kind: 'warning', okLabel: 'Discard changes', cancelLabel: 'Keep editing',
  }),
  protectClose: async (hasChanges: () => boolean) => {
    if (!isTauri()) return () => {};
    const window = getCurrentWindow();
    return window.onCloseRequested(async (event) => {
      if (!hasChanges()) return;
      event.preventDefault();
      if (await api.discardChanges()) await window.destroy();
    });
  },
};

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
