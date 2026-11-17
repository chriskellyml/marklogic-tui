import { invoke } from '@tauri-apps/api/core';
import type { Connection } from './api';
import { createRestDraft, restEntry, restMethods, type RestEntry, type RestMethod } from './rest-api';
import type { ApiOperation } from './openapi';
export interface RewriterSource { path: string; root: string; database: string; xml: string; message: string }
export interface RewriterRoute extends ApiOperation { pattern: string; target: string; notes: string[] }
export const rewriterApi = { load: (connection: Connection) => invoke<RewriterSource>('load_rewriter', { connection }) };
const RW = 'http://marklogic.com/xdmp/rewriter';
const REST = 'http://marklogic.com/appservices/rest';
const children = (el: Element) => Array.from(el.children);

// Convert common route shapes, never send a regular expression as a request URL.
export function routeShape(pattern: string): { path: string; params: RestEntry[] } {
  let source = pattern.replace(/^\^/, '').replace(/\$$/, '').replace(/\(\/\)\?$/, '').replace(/\/\?$/, '');
  const params: RestEntry[] = [];
  let path = '';
  for (let i = 0; i < source.length; i++) {
    const char = source[i];
    if (char === '(') {
      let end = i + 1, escaped = false, inClass = false;
      for (; end < source.length; end++) {
        const c = source[end];
        if (escaped) { escaped = false; continue; }
        if (c === '\\') { escaped = true; continue; }
        if (c === '[') inClass = true;
        if (c === ']') inClass = false;
        if (!inClass && c === '(') throw new Error('Nested regex groups need a manually entered request path.');
        if (!inClass && c === ')') break;
      }
      if (end === source.length) throw new Error('Unclosed route group.');
      const group = source.slice(i + 1, end);
      const choices = group.split('|');
      const literal = choices.every(value => /^[\w.-]+$/.test(value));
      const name = literal && choices.includes('v1') ? 'version' : `segment${params.length + 1}`;
      params.push({ ...restEntry(name, literal ? choices[0] : ''), required: true, description: literal ? `Choices: ${choices.join(', ')}` : `Must match (${group})` });
      path += `{${name}}`; i = end;
      if (source[i + 1] === '?') i++;
    } else if (char === '\\' && i + 1 < source.length && /[./_-]/.test(source[i + 1])) path += source[++i];
    else if ('.[]+*?|{}^$\\'.includes(char)) throw new Error('This regex needs a manually entered request path.');
    else path += char;
  }
  if (!path.startsWith('/')) throw new Error('Route does not define a relative HTTP path.');
  return { path: path || '/', params };
}

export function parseRewriter(xml: string): RewriterRoute[] {
  if (/<!DOCTYPE/i.test(xml)) throw new Error('DOCTYPE declarations are not supported in rewriter files.');
  const document = new DOMParser().parseFromString(xml, 'application/xml');
  if (document.getElementsByTagName('parsererror').length) throw new Error('The rewriter is not valid XML.');
  const root = document.documentElement;
  if (![RW, REST].includes(root.namespaceURI ?? '')) throw new Error('Unrecognized rewriter namespace.');
  const routes: RewriterRoute[] = [];
  interface Context { patterns: string[]; methods?: RestMethod[]; params: RestEntry[]; headers: RestEntry[]; notes: string[]; literal?: string }
  function emit(context: Context, target: string, key: string) {
    const pattern = context.patterns.at(-1) || context.literal || '/';
    const methods = context.methods ?? ['GET'];
    for (const method of methods) {
      const route: RewriterRoute = { id: JSON.stringify([key, method, pattern, target, context.params.map(p => [p.name, p.value]), context.headers.map(h => [h.name, h.value])]), name: `${method} ${pattern}`, path: pattern, method, group: '', pattern, target, notes: [...context.notes] };
      if (!context.methods) route.notes.push('No HTTP method restriction; starts with GET.');
      try {
        const shape = context.literal ? { path: context.literal, params: [] } : routeShape(pattern);
        const draft = createRestDraft();
        Object.assign(draft, { name: `${method} ${shape.path}`, path: shape.path, method, params: structuredClone(context.params), headers: structuredClone(context.headers), pathParams: shape.params, collection: 'Scratch' });
        route.draft = draft; route.name = draft.name; route.path = shape.path;
        route.group = shape.path.split('/').filter(part => part && !part.startsWith('{')).slice(0, 2).join('/') || 'Other routes';
        if (context.patterns.length > 1) route.notes.push(`Also constrained by: ${context.patterns.slice(0, -1).join(', ')}`);
      } catch (error) { route.error = error instanceof Error ? error.message : String(error); route.group = 'Manual paths'; }
      routes.push(route);
    }
  }
  function visit(el: Element, context: Context, key: string) {
    const ctx: Context = structuredClone(context);
    if (el.namespaceURI !== root.namespaceURI) return;
    const name = el.localName;
    if (name === 'match-path') {
      const pattern = el.getAttribute('matches') || el.getAttribute('match');
      if (pattern) { ctx.patterns.push(pattern); ctx.literal = undefined; }
      if (el.hasAttribute('prefix')) { ctx.literal = el.getAttribute('prefix')!; ctx.notes.push('Prefix route; append a suffix to the path if needed.'); }
      if (el.hasAttribute('any-of')) {
        for (const path of el.getAttribute('any-of')!.trim().split(/\s+/)) children(el).forEach((child, i) => visit(child, { ...ctx, literal: path, patterns: [...ctx.patterns, path] }, `${key}:${path}:${i}`));
        return;
      }
    }
    if (name === 'match-method') {
      const allowed = (el.getAttribute('any-of') ?? '').trim().split(/\s+/);
      ctx.methods = (ctx.methods ?? [...restMethods]).filter(method => allowed.includes(method));
    }
    if (name === 'match-query-param' || name === 'match-header' || name === 'match-accept' || name === 'match-content-type') {
      const field = name === 'match-query-param' ? 'params' : 'headers';
      const paramName = el.getAttribute('name') || (name === 'match-accept' ? 'Accept' : name === 'match-content-type' ? 'Content-Type' : '');
      if (paramName && !['authorization', 'host', 'content-length'].includes(paramName.toLowerCase())) {
        const value = el.getAttribute('value') ?? el.getAttribute('any-of')?.trim().split(/\s+/)[0] ?? '';
        ctx[field].push({ ...restEntry(paramName, value), required: Boolean(value), description: 'Rewriter matching condition; fill in a value if needed.' });
      }
    }
    if (['match-role', 'match-user', 'match-execute-privilege', 'match-cookie', 'match-string'].includes(name)) ctx.notes.push(`Additional server condition: ${new XMLSerializer().serializeToString(el.cloneNode(false))}`);
    if (name === 'dispatch') { emit(ctx, el.textContent?.trim() ?? '', key); return; }
    if (name === 'request' && el.namespaceURI === REST) {
      ctx.patterns.push(el.getAttribute('uri') || '/');
      const http = children(el).filter(child => child.localName === 'http');
      for (const [i, branch] of (http.length ? http : [el]).entries()) {
        const method = branch.getAttribute('method');
        const params = [...children(el), ...children(branch)].filter(child => child.localName === 'param').map(child => ({ ...restEntry(child.getAttribute('name') || '', child.getAttribute('default') || ''), required: child.getAttribute('required') === 'true', enabled: child.getAttribute('required') === 'true' || child.hasAttribute('default') }));
        emit({ ...ctx, methods: method ? restMethods.filter(m => method.split(/\s+/).includes(m)) : undefined, params }, el.getAttribute('endpoint') || '', `${key}:${i}`);
      }
      return;
    }
    children(el).forEach((child, i) => visit(child, ctx, `${key}:${i}`));
  }
  visit(root, { patterns: [], params: [], headers: [], notes: [] }, 'route');
  return routes;
}
