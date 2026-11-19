import type { Connection, ServerProfile } from './api';
import { defaultSessionId, sessionKey } from './workbench-sessions';

export interface Environment extends Connection { modulesDatabase: string }
const key = 'workbench.recent-environments.v1';
export function sameEnvironment(a: Pick<Connection, 'server' | 'appServer'>, b: Pick<Connection, 'server' | 'appServer'>) {
  return a.server === b.server && a.appServer === b.appServer;
}
export function readRecentEnvironments(sessionId = defaultSessionId): Environment[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(sessionKey(sessionId, key)) ?? '[]');
    if (!Array.isArray(value)) return [];
    return value.filter((item): item is Environment => item && typeof item.server === 'string' && !!item.server &&
      (item.appServer === null || typeof item.appServer === 'string') && typeof item.database === 'string' && typeof item.modulesDatabase === 'string').slice(0, 4);
  } catch { return []; }
}
export function saveRecentEnvironments(items: Environment[], sessionId = defaultSessionId) {
  try { localStorage.setItem(sessionKey(sessionId, key), JSON.stringify(items)); } catch { /* History remains available for this session. */ }
}
export function availableEnvironments(items: Environment[], servers: ServerProfile[], current: Connection) {
  return items.filter(item => !sameEnvironment(item, current) && servers.some(server => server.name === item.server &&
    (!item.appServer || server.appServers.some(endpoint => endpoint.name === item.appServer))));
}
export function rememberEnvironment(items: Environment[], current: Environment, target: Environment): Environment[] {
  return [current, ...items.filter(item => !sameEnvironment(item, current) && !sameEnvironment(item, target))].slice(0, 4);
}

export const activeEnvironmentKey = 'workbench.active-environment.v1';

export function readActiveEnvironment(servers: ServerProfile[], sessionId = defaultSessionId): Environment | null {
  try {
    const item: unknown = JSON.parse(localStorage.getItem(sessionKey(sessionId, activeEnvironmentKey)) ?? 'null');
    if (!item || typeof item !== 'object') return null;
    const env = item as Partial<Environment>;
    if (typeof env.server !== 'string' || !(env.appServer === null || typeof env.appServer === 'string') ||
      typeof env.database !== 'string' || !env.database.trim() || typeof env.modulesDatabase !== 'string') return null;
    const server = servers.find(candidate => candidate.name === env.server);
    if (!server || (env.appServer && !server.appServers.some(endpoint => endpoint.name === env.appServer))) return null;
    return env as Environment;
  } catch { return null; }
}

export function saveActiveEnvironment(environment: Environment, sessionId = defaultSessionId) {
  try { localStorage.setItem(sessionKey(sessionId, activeEnvironmentKey), JSON.stringify(environment)); } catch { /* Selection remains available for this session. */ }
}

export function environmentColor(server: ServerProfile | undefined): string {
  return /^#[0-9a-f]{6}$/i.test(server?.color ?? '') ? server!.color! : '#ffcc00';
}

export function environmentStripeColors(server: ServerProfile | undefined): [string, string] {
  const first = /^#[0-9a-f]{6}$/i.test(server?.color ?? '') ? server!.color! : '#ffcc00';
  const second = /^#[0-9a-f]{6}$/i.test(server?.stripeColorSecondary ?? '') ? server!.stripeColorSecondary! : '#101418';
  return [first, second];
}
