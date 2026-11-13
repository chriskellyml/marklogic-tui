import { invoke } from '@tauri-apps/api/core';
import { confirm } from '@tauri-apps/plugin-dialog';
import type { Connection } from './api';
export interface ModuleContext { connection: Connection; database: string; uri: string }
export interface CachedModule { uri: string; dirty: boolean; unsynced: boolean }
export const modulesApi = {
  cache: (connection: Connection, database: string) => invoke<CachedModule[]>('module_cache',{connection,database}),
  search: (connection: Connection, database: string, pattern: string, offset: number) => invoke<{uris:string[];total:number}>('search_modules',{connection,database,pattern,offset}),
  checkout: (context: ModuleContext) => invoke<void>('checkout_module',{context}),
  open: (context: ModuleContext) => invoke<{content:string;original:string}>('open_module',{context}),
  save: (context: ModuleContext, content: string, expected: string) => invoke<void>('save_module',{context,content,expected}),
  drop: (context: ModuleContext, expected: string) => invoke<{content:string;original:string}>('drop_module_edits',{context,expected}),
  confirmDrop: (context: ModuleContext) => confirm(`Drop all local edits to ${context.uri} and download the current server copy from ${context.connection.server} / ${context.database}?`,{title:'Drop module edits',kind:'warning',okLabel:'Drop local edits',cancelLabel:'Keep edits'}),
  push: (context: ModuleContext, expected: string) => invoke<void>('push_module',{context,expected}),
  confirmPush: (context: ModuleContext) => confirm(`Write ${context.uri} to ${context.database} on ${context.connection.server} (${context.connection.appServer ?? 'profile default'})?`,{title:'Sync module to database',kind:'warning',okLabel:'Sync to database',cancelLabel:'Cancel'}),
};
export function moduleLanguage(uri: string): 'javascript'|'xquery'|'text' { return /\.(sjs|mjs|js)$/i.test(uri)?'javascript':/\.xqy$/i.test(uri)?'xquery':'text'; }
