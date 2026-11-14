import { beforeEach, expect, it } from 'vitest';
import { availableEnvironments, readRecentEnvironments, rememberEnvironment, saveRecentEnvironments, type Environment } from './recent-environments';

const environment = (server: string, appServer: string | null = null): Environment => ({ server, appServer, database: `${server}-content`, modulesDatabase: `${server}-modules` });
beforeEach(() => localStorage.clear());

it('keeps four unique recent pairs and persists their database selections', () => {
  const [a, b, c, d, e, f] = ['a', 'b', 'c', 'd', 'e', 'f'].map(name => environment(name));
  let history: Environment[] = [];
  for (const [from, to] of [[a,b], [b,c], [c,d], [d,e], [e,f]]) history = rememberEnvironment(history, from, to);
  expect(history.map(item => item.server)).toEqual(['e', 'd', 'c', 'b']);
  history = rememberEnvironment(history, f, c);
  expect(history.map(item => item.server)).toEqual(['f', 'e', 'd', 'b']);
  saveRecentEnvironments(history);
  expect(readRecentEnvironments()).toEqual(history);
});

it('omits the current pair and removed profiles or app servers', () => {
  const servers = [{ name: 'local', endpoint: '', appServers: [{ name: 'api', port: 8000, secure: false }] }];
  expect(availableEnvironments([environment('local'), environment('local', 'api'), environment('local', 'removed'), environment('gone')], servers, environment('local')))
    .toEqual([environment('local', 'api')]);
});

it('restores a valid active selection and ignores removed endpoints or malformed data', async () => {
  const { readActiveEnvironment, saveActiveEnvironment } = await import('./recent-environments');
  const servers = [{ name: 'local', endpoint: '', appServers: [{ name: 'api', port: 8000, secure: false }] }];
  const selected = environment('local', 'api');
  saveActiveEnvironment(selected);
  expect(readActiveEnvironment(servers)).toEqual(selected);
  expect(readActiveEnvironment([{ ...servers[0], appServers: [] }])).toBeNull();
  localStorage.setItem('workbench.active-environment.v1', '{');
  expect(readActiveEnvironment(servers)).toBeNull();
});
