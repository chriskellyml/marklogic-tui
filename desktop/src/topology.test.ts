import { describe, expect, it } from 'vitest';
import { applicationName, groupForestsByDatabaseType, indexTopology, projectTopology } from './topology';
import { topologyFixture, topologyWithDatabaseTypes } from './topology-fixture';

describe('configuration topology', () => {
  it('uses first tokens only for the app server navigator', () => {
    expect(applicationName('FOO-core')).toBe('FOO');
    expect(applicationName('FOO_service')).toBe('FOO');
    expect(applicationName('App Services')).toBe('App');
    const index = indexTopology(topologyFixture);
    expect(index.applications.get('FOO')?.map(app => app.id)).toEqual(['a1', 'a2']);
    const result = projectTopology(topologyFixture, index, { kind: 'application', id: 'FOO' });
    expect(result.databases.map(db => db.name)).toEqual(['Archive', 'Orders', 'Shared-code', 'Security']);
    expect(result.forests.map(f => f.id)).toEqual(expect.arrayContaining(['p', 'r1', 'r2']));
    expect(result.forests.map(f => f.id)).not.toContain('other');
    expect(result.forests.map(f => f.id)).not.toContain('orphan');
  });
  it('groups forest families by configured database role, including shared roles and unclassified forests', () => {
    const snapshot = topologyWithDatabaseTypes();
    // Shared-code serves both modules and schemas; its name determines neither.
    snapshot.databases.find(db => db.id === '9007199254740993')!.schemaDatabaseId = 'modules';
    const index = indexTopology(snapshot);
    const projection = projectTopology(snapshot, index, { kind: 'all' });
    const rows = groupForestsByDatabaseType(projection.forests, index);
    expect(rows.map(row => row.type)).toEqual(['Content', 'Modules', 'Schemas', 'Triggers', 'Security', 'Other']);
    const ids = (type: string) => rows.find(row => row.type === type)!.forests.map(forest => forest.id);
    expect(ids('Content')).toEqual(expect.arrayContaining(['p', 'r1', 'r2']));
    expect(ids('Content')).not.toContain('schema-copy');
    expect(ids('Modules')).toEqual(['code']);
    expect(ids('Schemas')).toEqual(expect.arrayContaining(['schema', 'schema-copy', 'code']));
    expect(ids('Triggers')).toEqual(['triggers']);
    expect(ids('Security')).toEqual(['sec']);
    expect(ids('Other')).toEqual(['orphan']);
    expect(index.forestDatabaseIds.get('schema-copy')).toEqual(new Set(['schema']));
  });
  it('pairs arbitrarily named forests using configured replica IDs', () => {
    const { symbols, masters } = indexTopology(topologyFixture);
    const primary = symbols.get('p')!;
    expect(primary.replica).toBe(false);
    expect(symbols.get('r1')).toMatchObject({ replica: true, family: 'p', hue: primary.hue });
    expect(symbols.get('r2')).toMatchObject({ replica: true, family: 'p', hue: primary.hue });
    expect(symbols.get('r1')!.label).not.toBe(symbols.get('r2')!.label);
    expect(masters.get('r1')).toEqual(['p']);
  });
  it('keeps adjacent 64-bit database IDs distinct and follows shared cycles once', () => {
    const index = indexTopology(topologyFixture);
    const result = projectTopology(topologyFixture, index, { kind: 'server', id: 'a1' });
    expect(result.databases.map(db => db.name)).toEqual(['Orders', 'Shared-code', 'Security']);
    expect(result.servers.map(app => app.id)).toEqual(['a1']);
    expect(result.hosts.map(host => host.id)).toEqual(['h2', 'h3', 'h1']);
    expect(result.warnings).toEqual([]);
  });
  it('shows databases without app servers and detached forests in the cluster', () => {
    const index = indexTopology(topologyFixture);
    expect(projectTopology(topologyFixture, index, { kind: 'database', id: 'detached' }).servers).toEqual([]);
    const all = projectTopology(topologyFixture, index, { kind: 'all' });
    expect(all.forests.map(f => f.id)).toContain('orphan');
    expect(all.databases).toHaveLength(topologyFixture.databases.length);
  });
  it('includes shared-disk failover and reports missing links and colocated replicas', () => {
    const snapshot = structuredClone(topologyFixture);
    snapshot.forests.find(f => f.id === 'p')!.failoverHostIds = ['h1'];
    snapshot.forests.find(f => f.id === 'r1')!.hostId = 'h2';
    snapshot.databases[0].schemaDatabaseId = 'missing';
    const result = projectTopology(snapshot, indexTopology(snapshot), { kind: 'server', id: 'a1' });
    expect(result.hosts.map(host => host.id)).toContain('h1');
    expect(result.warnings).toContain('Database missing is linked but missing from this snapshot.');
    expect(result.warnings).toContain('misleading-replica-9 has a replica configured on the same host.');
  });
});
