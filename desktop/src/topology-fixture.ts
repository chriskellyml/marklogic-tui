import type { TopologyAppServer, TopologyDatabase, TopologyForest, TopologySnapshot } from './topology-api';
const app = (id: string, name: string, contentDatabaseId: string, port: number): TopologyAppServer => ({
  id, name, contentDatabaseId, port, groupId: 'g1', hostIds: ['h1'], kind: 'http',
  modulesDatabaseId: 'modules', secure: false, enabled: true, root: '/', authentication: 'digest',
});
const db = (id: string, name: string, forestIds: string[]): TopologyDatabase => ({
  id, name, forestIds, securityDatabaseId: 'security', schemaDatabaseId: '0', triggersDatabaseId: '0',
});
const forest = (id: string, name: string, hostId: string, replicaIds: string[] = []): TopologyForest => ({
  id, name, hostId, replicaIds, failoverHostIds: [], dataDirectory: '', enabled: true,
});
// Deliberately misleading names: relationships must follow IDs, including those
// too large for a lossless JavaScript number.
export const topologyFixture: TopologySnapshot = {
  capturedAt: '2026-11-14T12:00:00Z',
  groups: [{ id: 'g1', name: 'Evaluators' }, { id: 'g2', name: 'Data' }],
  hosts: [{ id: 'h1', name: 'eval-01', groupId: 'g1' }, { id: 'h2', name: 'data-01', groupId: 'g2' }, { id: 'h3', name: 'data-02', groupId: 'g2' }],
  appServers: [app('a1', 'FOO-core', '9007199254740992', 8100), app('a2', 'FOO-service', '9007199254740993', 8101), app('a3', 'BAR-api', 'other', 8200)],
  databases: [db('9007199254740992', 'Orders', ['p']), db('9007199254740993', 'Archive', ['archive']), db('modules', 'Shared-code', ['code']), db('security', 'Security', ['sec']), db('other', 'Unrelated', ['other']), db('detached', 'No-app-server', [])],
  forests: [forest('p', 'misleading-replica-9', 'h2', ['r1', 'r2']), forest('r1', 'arbitrary-copy', 'h3'), forest('r2', 'unrelated-name', 'h1'), forest('archive', 'Archive', 'h2'), forest('code', 'Code', 'h2'), forest('sec', 'Security', 'h2'), forest('other', 'Other', 'h3'), forest('orphan', 'Detached', 'h3')],
};

export function topologyWithDatabaseTypes(): TopologySnapshot {
  const snapshot = structuredClone(topologyFixture);
  snapshot.databases[0].schemaDatabaseId = 'schema';
  snapshot.databases[0].triggersDatabaseId = 'triggers';
  snapshot.databases.push(db('schema', 'Lookup', ['schema']), db('triggers', 'Actions', ['triggers']));
  snapshot.forests.push(forest('schema', 'Vocabulary', 'h2', ['schema-copy']), forest('schema-copy', 'Extra', 'h3'), forest('triggers', 'Jobs', 'h2'));
  snapshot.forests.find(item => item.id === 'schema')!.failoverHostIds = ['h1'];
  return snapshot;
}
