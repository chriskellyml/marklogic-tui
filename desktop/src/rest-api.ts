import { invoke } from '@tauri-apps/api/core';
import type { Connection, ServerProfile } from './api';
import { formatDocumentJson } from './format-document-json';
import { formatXml } from './log-detail';

export const restMethods = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'] as const;
export type RestMethod = typeof restMethods[number];
export interface RestEntry { id: string; enabled: boolean; name: string; value: string; required?: boolean; description?: string; serialization?: 'repeat'|'comma'|'space'|'pipe'|'deepObject' }
export interface RestSet { pathParams?: RestEntry[]; id: string; name: string; params: RestEntry[]; headers: RestEntry[]; body: string; bodyEnabled: boolean }
export interface RestDraft {
  id: string; name: string; method: RestMethod; path: string;
  params: RestEntry[]; headers: RestEntry[]; body: string; bodyEnabled: boolean;
  pathParams?: RestEntry[];
  collection?: string; sets?: RestSet[];
}
export interface RestRequest { method: RestMethod; path: string; headers: [string, string][]; body: string | null }
export interface RestCredentials { username: string; password: string; authType?: 'basic' | 'digest' | 'digestbasic' }
export interface RestCurlExport { path: string; script: string }
export interface RestResponse {
  url: string; status: number; statusText: string; headers: [string, string][];
  body: string; elapsedMs: number; size: number; truncated: boolean;
}
export const restApi = {
  execute: (connection: Connection, request: RestRequest, credentials?: RestCredentials) => invoke<RestResponse>('execute_rest_request', { connection, request, credentials }),
  exportCurl: (connection: Connection, request: RestRequest, credentials?: RestCredentials) => invoke<RestCurlExport>('export_rest_curl', { connection, request, credentials }),
  openCurl: (path: string) => invoke<void>('open_rest_curl', { path }),
};
export const restCollectionsKey = 'workbench.rest-collections.v1';
export function readRestCollections(): string[] {
  try { const value: unknown = JSON.parse(localStorage.getItem(restCollectionsKey) ?? '[]'); if (Array.isArray(value)) return ['Scratch', ...value.filter((item): item is string => typeof item === 'string' && Boolean(item.trim()) && item !== 'Scratch')]; } catch { /* Use Scratch. */ }
  return ['Scratch'];
}
export const restStorageKey = 'workbench.rest-requests.v1';
export const restEntry = (name = '', value = ''): RestEntry => ({ id: crypto.randomUUID(), enabled: true, name, value });

export const restTemplates = [
  { id: 'search', name: 'Search documents', method: 'GET', path: '/v1/search', params: [['q', ''], ['format', 'json'], ['pageLength', '10']] },
  { id: 'document', name: 'Read a document', method: 'GET', path: '/v1/documents', params: [['uri', '/example.json']] },
  { id: 'structured', name: 'Structured search', method: 'POST', path: '/v1/search', params: [['format', 'json']], body: '{\n  "search": {\n    "query": {\n      "collection-query": { "uri": ["example"] }\n    }\n  }\n}' },
  { id: 'write', name: 'Write a document', method: 'PUT', path: '/v1/documents', params: [['uri', '/example.json']], body: '{\n  "message": "Hello, MarkLogic"\n}' },
] satisfies { id: string; name: string; method: RestMethod; path: string; params: string[][]; body?: string }[];

export function createRestDraft(templateId?: string): RestDraft {
  const template = restTemplates.find(item => item.id === templateId);
  const body = template && 'body' in template ? template.body ?? '' : '';
  return {
    id: crypto.randomUUID(), name: template?.name ?? 'Untitled request', method: template?.method ?? 'GET',
    path: template?.path ?? '/v1/', params: template?.params.map(([name, value]) => restEntry(name, value)) ?? [],
    headers: [restEntry('Accept', 'application/json'), ...(body ? [restEntry('Content-Type', 'application/json')] : [])],
    body, bodyEnabled: Boolean(body),
  };
}

export function restBaseUrl(server: ServerProfile | undefined, appServer: string | null): string {
  if (!server) return '';
  try {
    const url = new URL(server.endpoint);
    const endpoint = server.appServers.find(item => item.name === appServer);
    if (endpoint) { url.protocol = endpoint.secure ? 'https:' : 'http:'; url.port = String(endpoint.port); }
    return url.origin;
  } catch { return ''; }
}

export function pathParameters(path: string, entries: RestEntry[] = []): RestEntry[] {
  const names = [...new Set(Array.from(path.matchAll(/\{([^{}]+)\}/g), match => match[1]))];
  return [...entries, ...names.filter(name => !entries.some(entry => entry.name === name)).map(name => ({ ...restEntry(name), id: `path:${name}`, required: true }))];
}

export function buildRestRequest(draft: RestDraft, variables: Record<string, string> = {}): RestRequest {
  const resolve = (value: string) => value.replace(/%%([^%]+)%%/g, (_match, name: string) => {
    if (!Object.prototype.hasOwnProperty.call(variables, name)) throw new Error(`Variable ${name} is not defined.`);
    return variables[name];
  });
  let path = resolve(draft.path).trim();
  for (const entry of draft.pathParams ?? []) {
    const name = resolve(entry.name);
    if (!path.includes(`{${name}}`)) continue;
    const value = resolve(entry.value);
    if (!entry.enabled || !value.trim()) throw new Error(`Path parameter ${name} is required.`);
    path = path.replaceAll(`{${name}}`, encodeURIComponent(value));
  }
  if (/\{[^}]+\}/.test(path)) throw new Error('Fill in the path parameters before sending.');
  for (const entry of [...draft.params, ...draft.headers]) if (entry.required && (!entry.enabled || !resolve(entry.value).trim())) throw new Error(`Parameter ${entry.name} is required.`);
  if (!path.startsWith('/') || path.startsWith('//') || /[\\#]/.test(path)) {
    throw new Error('Use a relative path beginning with /, such as /v1/search. The environment supplies the base URL.');
  }
  const params = new URLSearchParams();
  for (const entry of draft.params) if (entry.enabled && entry.name.trim()) {
    const name = resolve(entry.name.trim()), valueText = resolve(entry.value);
    if (entry.serialization && valueText) {
      let value: unknown;
      try { value = JSON.parse(valueText); } catch { throw new Error(`Parameter ${name} needs a JSON array or object.`); }
      if (entry.serialization === 'deepObject' && value && typeof value === 'object' && !Array.isArray(value)) {
        for (const [key, item] of Object.entries(value)) params.append(`${name}[${key}]`, String(item));
      } else if (Array.isArray(value)) {
        if (entry.serialization === 'repeat') for (const item of value) params.append(name, String(item));
        else params.append(name, value.join(entry.serialization === 'space' ? ' ' : entry.serialization === 'pipe' ? '|' : ','));
      } else throw new Error(`Parameter ${entry.name} needs a JSON array or object.`);
    } else params.append(name, valueText);
  }
  const query = params.toString();
  return {
    method: draft.method, path: path + (query ? `${path.includes('?') ? (path.endsWith('?') || path.endsWith('&') ? '' : '&') : '?'}${query}` : ''),
    headers: draft.headers.filter(entry => entry.enabled && entry.name.trim()).map(entry => [resolve(entry.name.trim()), resolve(entry.value)]),
    body: draft.bodyEnabled ? resolve(draft.body) : null,
  };
}

function isEntry(value: unknown): value is RestEntry {
  if (!value || typeof value !== 'object') return false;
  const entry = value as Partial<RestEntry>;
  return typeof entry.id === 'string' && typeof entry.enabled === 'boolean' && typeof entry.name === 'string' && typeof entry.value === 'string';
}
function isDraft(value: unknown): value is RestDraft {
  if (!value || typeof value !== 'object') return false;
  const draft = value as Partial<RestDraft>;
  return typeof draft.id === 'string' && typeof draft.name === 'string' && restMethods.includes(draft.method as RestMethod) &&
    typeof draft.path === 'string' && typeof draft.body === 'string' && typeof draft.bodyEnabled === 'boolean' &&
    Array.isArray(draft.params) && draft.params.every(isEntry) && Array.isArray(draft.headers) && draft.headers.every(isEntry);
}
export function readRestDrafts(): RestDraft[] {
  try {
    const saved: unknown = JSON.parse(localStorage.getItem(restStorageKey) ?? 'null');
    if (Array.isArray(saved) && saved.every(isDraft) && new Set(saved.map(item => item.id)).size === saved.length) return saved;
  } catch { /* Start with a sample when browser storage is unavailable. */ }
  return [createRestDraft('search')];
}

export function detectRestBody(body: string): 'json' | 'xml' | 'text' {
  const value = body.trim();
  if (!value) return 'text';
  try { JSON.parse(value); return 'json'; } catch { /* Try XML. */ }
  if (value.startsWith('<')) {
    try { if (!new DOMParser().parseFromString(value, 'application/xml').querySelector('parsererror')) return 'xml'; } catch { /* Plain text. */ }
  }
  return 'text';
}
export function prettyRestBody(body: string): string {
  const format = detectRestBody(body);
  return format === 'json' ? formatDocumentJson(body) : format === 'xml' ? formatXml(body) : body;
}
