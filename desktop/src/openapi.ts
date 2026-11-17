import { invoke } from '@tauri-apps/api/core';
import type { Connection } from './api';
import { createRestDraft, restEntry, restMethods, type RestDraft, type RestEntry, type RestMethod } from './rest-api';

type Obj = Record<string, any>;
export interface ApiOperation { id: string; method: RestMethod; path: string; name: string; group: string; draft?: RestDraft; error?: string }
export interface ParsedSpec { title: string; operations: ApiOperation[] }
export const openapiApi = {
  discover: (connection: Connection, database: string) => invoke<string[]>('discover_openapi', { connection, database }),
  load: (connection: Connection, database: string, uri: string) => invoke<unknown>('load_openapi', { connection, database, uri }),
};
export function parseOpenApi(input: unknown): ParsedSpec {
  const doc = input as Obj;
  if (!doc || typeof doc !== 'object' || !(String(doc.openapi ?? '').startsWith('3.') || doc.swagger === '2.0') || !doc.paths) throw new Error('Expected OpenAPI 3.x or Swagger 2.0 with paths.');
  function resolve(value: any, seen = new Set<string>()): Obj {
    if (!value || typeof value !== 'object') return {};
    if (!value.$ref) return value;
    const ref = value.$ref as string;
    if (!ref.startsWith('#/')) throw new Error(`External reference is not supported: ${ref}. Bundle this spec first.`);
    if (seen.has(ref)) throw new Error(`Circular reference: ${ref}`);
    const target = ref.slice(2).split('/').reduce((v: any, key: string) => v?.[key.replace(/~1/g, '/').replace(/~0/g, '~')], doc);
    if (!target) throw new Error(`Missing reference: ${ref}`);
    return { ...resolve(target, new Set([...seen, ref])), ...Object.fromEntries(Object.entries(value).filter(([key]) => key !== '$ref')) };
  }
  function sample(raw: any, depth = 0): any {
    if (depth > 8) return null;
    const schema = resolve(raw);
    if (schema.example !== undefined) return schema.example;
    if (schema.default !== undefined) return schema.default;
    if (schema.enum?.length) return schema.enum[0];
    if (schema.allOf) return Object.assign({}, ...schema.allOf.map((s: any) => sample(s, depth + 1)));
    if (schema.oneOf || schema.anyOf) return sample((schema.oneOf || schema.anyOf)[0], depth + 1);
    if (schema.type === 'array') return [sample(schema.items, depth + 1)];
    if (schema.properties || schema.type === 'object') return Object.fromEntries(Object.entries(schema.properties ?? {}).filter(([, v]) => !resolve(v).readOnly).map(([key, value]) => [key, sample(value, depth + 1)]));
    return schema.type === 'boolean' ? false : schema.type === 'integer' || schema.type === 'number' ? 0 : '';
  }
  const valueText = (value: any): string => value == null ? '' : typeof value === 'object' ? JSON.stringify(value) : String(value);
  const operations: ApiOperation[] = [];
  for (const [path, rawPath] of Object.entries(doc.paths)) {
    const item = resolve(rawPath);
    for (const method of restMethods) {
      if (!item[method.toLowerCase()]) continue;
      const op = resolve(item[method.toLowerCase()]);
      const operation: ApiOperation = { id: `${method} ${path}`, method, path, name: op.summary || op.operationId || path, group: op.tags?.[0] || 'Endpoints' };
      try {
        const draft = createRestDraft();
        draft.name = operation.name; draft.method = method; draft.params = []; draft.headers = []; draft.pathParams = [];
        const server = (op.servers ?? item.servers ?? doc.servers)?.[0];
        let basePath = doc.basePath || '';
        if (server) {
          const serverUrl = String(server.url).replace(/\{([^}]+)\}/g, (_: string, name: string) => {
            const value = server.variables?.[name]?.default;
            if (value === undefined) throw new Error(`Server variable ${name} needs a default`);
            return String(value);
          });
          basePath = new URL(serverUrl, 'http://environment.invalid/').pathname;
        }
        draft.path = `${basePath.replace(/\/$/, '')}/${path.replace(/^\//, '')}`;
        const params = new Map<string, Obj>();
        for (const raw of [...(item.parameters ?? []), ...(op.parameters ?? [])]) { const p = resolve(raw); params.set(`${p.in}:${p.name}`, p); }
        let swaggerBody: Obj | undefined;
        for (const p of params.values()) {
          if (p.in === 'body') { swaggerBody = p; continue; }
          if (p.in === 'formData') throw new Error('Swagger formData needs manual setup.');
          if (p.in === 'header' && ['authorization', 'host', 'content-length'].includes(String(p.name).toLowerCase())) continue;
          const schema = resolve(p.schema ?? p);
          const example = p.example ?? resolve(Object.values(p.examples ?? {})[0]).value ?? schema.example ?? schema.default ?? schema.enum?.[0];
          const entry: RestEntry = { ...restEntry(p.name, valueText(example)), enabled: Boolean(p.required || example !== undefined), required: Boolean(p.required), description: p.description };
          if (p.in === 'path' && Array.isArray(example)) entry.value = example.join(',');
          if (p.in === 'header' && Array.isArray(example)) entry.value = example.join(',');
          if (p.in === 'path') { entry.required = true; draft.pathParams.push(entry); }
          else if (p.in === 'query') {
            if (schema.type === 'array') entry.serialization = p.collectionFormat === 'multi' || (!doc.swagger && (p.style ?? 'form') === 'form' && p.explode !== false) ? 'repeat' : p.collectionFormat === 'ssv' || p.style === 'spaceDelimited' ? 'space' : p.collectionFormat === 'pipes' || p.style === 'pipeDelimited' ? 'pipe' : 'comma';
            if (schema.type === 'object') { if (p.style !== 'deepObject') throw new Error(`Object parameter ${p.name} requires manual serialization.`); entry.serialization = 'deepObject'; }
            draft.params.push(entry);
          }
          else if (p.in === 'header') draft.headers.push(entry);
          else if (p.in === 'cookie') draft.headers.push({ ...entry, name: 'Cookie', value: `${p.name}=${encodeURIComponent(entry.value)}` });
        }
        const body = resolve(op.requestBody);
        const mediaTypes = Object.keys(body.content ?? {});
        const contentType = mediaTypes.find(type => /json/.test(type)) || mediaTypes[0] || (swaggerBody ? (op.consumes ?? doc.consumes ?? ['application/json'])[0] : '');
        if (contentType) {
          if (contentType === 'multipart/form-data') throw new Error('Multipart file upload needs manual setup.');
          const media = resolve(body.content?.[contentType]);
          const example = media.example ?? resolve(Object.values(media.examples ?? {})[0]).value ?? sample(media.schema ?? swaggerBody?.schema);
          draft.body = contentType.includes('json') ? JSON.stringify(example, null, 2) : contentType === 'application/x-www-form-urlencoded' && typeof example === 'object' ? new URLSearchParams(Object.entries(example ?? {}).map(([key, value]) => [key, valueText(value)])).toString() : valueText(example);
          draft.bodyEnabled = true; draft.headers.push(restEntry('Content-Type', contentType));
        }
        const response = resolve(Object.entries(op.responses ?? {}).find(([status]) => status.startsWith('2'))?.[1]);
        const accept = Object.keys(response.content ?? {})[0] || (op.produces ?? doc.produces)?.[0];
        if (accept) draft.headers.push(restEntry('Accept', accept));
        draft.collection = 'Scratch';
        operation.draft = draft;
      } catch (error) { operation.error = error instanceof Error ? error.message : String(error); }
      operations.push(operation);
    }
  }
  return { title: doc.info?.title || 'OpenAPI', operations };
}
