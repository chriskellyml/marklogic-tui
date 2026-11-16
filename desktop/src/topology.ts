import type { TopologyDatabase, TopologyForest, TopologySnapshot } from './topology-api';

export const DATABASE_TYPES = ['Content', 'Modules', 'Schemas', 'Triggers', 'Security', 'Other'] as const;
export type DatabaseType = typeof DATABASE_TYPES[number];

export type TopologyScope = { kind: 'all' } | { kind: 'application' | 'server' | 'database'; id: string };
export const naturalCompare = (a: string, b: string) => a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });
export const applicationName = (name: string) => name.split(/[-_\s]+/).find(Boolean) || name;
export const hasLink = (id: string) => Boolean(id && id !== '0');
export function databaseLinks(database: TopologyDatabase) {
  return [
    { role: 'Security', id: database.securityDatabaseId },
    { role: 'Schemas', id: database.schemaDatabaseId },
    { role: 'Triggers', id: database.triggersDatabaseId },
  ].filter(link => hasLink(link.id));
}

export function indexTopology(snapshot: TopologySnapshot) {
  const byId = <T extends { id: string }>(items: T[]) => new Map(items.map(item => [item.id, item]));
  const groups = byId(snapshot.groups), hosts = byId(snapshot.hosts), databases = byId(snapshot.databases), forests = byId(snapshot.forests);
  const databaseTypes = new Map<string, Set<DatabaseType>>();
  const addType = (id: string, type: DatabaseType) => {
    if (!hasLink(id)) return;
    const types = databaseTypes.get(id) ?? new Set<DatabaseType>();
    types.add(type); databaseTypes.set(id, types);
  };
  for (const app of snapshot.appServers) {
    addType(app.contentDatabaseId, 'Content');
    addType(app.modulesDatabaseId, 'Modules');
  }
  for (const db of snapshot.databases) {
    addType(db.schemaDatabaseId, 'Schemas');
    addType(db.triggersDatabaseId, 'Triggers');
    addType(db.securityDatabaseId, 'Security');
  }
  // Roles describe configured usage. A shared database can serve several roles;
  // retain each one instead of guessing a single type from its name.
  for (const db of snapshot.databases) if (!databaseTypes.has(db.id)) addType(db.id, 'Other');
  const forestDatabaseIds = new Map<string, Set<string>>();
  for (const db of snapshot.databases) {
    const pending = [...db.forestIds], seen = new Set<string>();
    while (pending.length) {
      const id = pending.pop()!;
      if (seen.has(id)) continue;
      seen.add(id);
      const owners = forestDatabaseIds.get(id) ?? new Set<string>();
      owners.add(db.id); forestDatabaseIds.set(id, owners);
      pending.push(...(forests.get(id)?.replicaIds ?? []));
    }
  }
  const applications = new Map<string, typeof snapshot.appServers>();
  for (const app of [...snapshot.appServers].sort((a, b) => naturalCompare(a.name, b.name))) {
    const name = applicationName(app.name);
    const members = applications.get(name) ?? [];
    members.push(app); applications.set(name, members);
  }
  const masters = new Map<string, string[]>();
  for (const forest of snapshot.forests) for (const replica of forest.replicaIds) {
    const parents = masters.get(replica) ?? [];
    parents.push(forest.id); masters.set(replica, parents);
  }
  // Numbers belong to actual forest IDs, never to name suffixes. A replica
  // inherits its configured primary's number even if neither name hints at it.
  const symbols = new Map<string, { label: string; family: string; replica: boolean; hue: number }>();
  const sorted = [...snapshot.forests].sort((a, b) => naturalCompare(a.name, b.name) || naturalCompare(a.id, b.id));
  let number = 0;
  for (const forest of sorted.filter(item => !masters.has(item.id))) {
    number++;
    const hue = Math.round((number * 137.508) % 360);
    symbols.set(forest.id, { label: `F${number}p`, family: forest.id, replica: false, hue });
    forest.replicaIds.forEach((id, index) => symbols.set(id, {
      label: `F${number}r${forest.replicaIds.length > 1 ? index + 1 : ''}`, family: forest.id, replica: true, hue,
    }));
  }
  for (const forest of sorted) if (!symbols.has(forest.id)) {
    number++;
    symbols.set(forest.id, { label: `F${number}?`, family: forest.id, replica: masters.has(forest.id), hue: Math.round((number * 137.508) % 360) });
  }
  return { groups, hosts, databases, forests, applications, masters, symbols, databaseTypes, forestDatabaseIds };
}
export type TopologyIndex = ReturnType<typeof indexTopology>;

export function groupForestsByDatabaseType(forests: TopologyForest[], index: TopologyIndex) {
  return DATABASE_TYPES.map(type => ({
    type,
    forests: forests.filter(forest => {
      const owners = index.forestDatabaseIds.get(forest.id);
      if (!owners?.size) return type === 'Other';
      return [...owners].some(id => index.databaseTypes.get(id)?.has(type));
    }),
  })).filter(row => row.forests.length > 0);
}
export type ForestTypeRow = ReturnType<typeof groupForestsByDatabaseType>[number];

export function projectTopology(snapshot: TopologySnapshot, index: TopologyIndex, scope: TopologyScope) {
  const servers = snapshot.appServers.filter(app => scope.kind === 'all' ||
    (scope.kind === 'application' && applicationName(app.name) === scope.id) ||
    (scope.kind === 'server' && app.id === scope.id) ||
    (scope.kind === 'database' && (app.contentDatabaseId === scope.id || app.modulesDatabaseId === scope.id)));
  const directIds = new Set<string>();
  if (scope.kind === 'all') snapshot.databases.forEach(db => directIds.add(db.id));
  else if (scope.kind === 'database') directIds.add(scope.id);
  else for (const app of servers) for (const id of [app.contentDatabaseId, app.modulesDatabaseId]) if (hasLink(id)) directIds.add(id);
  // Database links may be shared or cyclic (Security commonly links to itself).
  const databaseIds = new Set(directIds), pending = [...directIds];
  while (pending.length) {
    const db = index.databases.get(pending.pop()!);
    if (!db) continue;
    for (const { id } of databaseLinks(db)) if (!databaseIds.has(id)) { databaseIds.add(id); pending.push(id); }
  }
  const forestIds = new Set<string>();
  if (scope.kind === 'all') snapshot.forests.forEach(forest => forestIds.add(forest.id));
  for (const id of databaseIds) index.databases.get(id)?.forestIds.forEach(forest => forestIds.add(forest));
  const forestQueue = [...forestIds];
  while (forestQueue.length) {
    const id = forestQueue.pop()!;
    const linked = [...(index.forests.get(id)?.replicaIds ?? []), ...(index.masters.get(id) ?? [])];
    for (const related of linked) if (!forestIds.has(related)) { forestIds.add(related); forestQueue.push(related); }
  }
  const hostIds = new Set(servers.flatMap(app => app.hostIds));
  for (const id of forestIds) {
    const forest = index.forests.get(id);
    if (forest) { hostIds.add(forest.hostId); forest.failoverHostIds.forEach(host => hostIds.add(host)); }
  }
  if (scope.kind === 'all') snapshot.hosts.forEach(host => hostIds.add(host.id));
  const warnings: string[] = [];
  for (const [kind, ids, collection] of [
    ['Database', databaseIds, index.databases], ['Forest', forestIds, index.forests], ['Host', hostIds, index.hosts],
  ] as const) for (const id of ids) if (!collection.has(id)) warnings.push(`${kind} ${id} is linked but missing from this snapshot.`);
  for (const id of forestIds) {
    const forest = index.forests.get(id);
    if (forest && forest.replicaIds.some(replica => index.forests.get(replica)?.hostId === forest.hostId)) {
      warnings.push(`${forest.name} has a replica configured on the same host.`);
    }
  }
  return {
    servers: [...servers].sort((a, b) => a.port - b.port || naturalCompare(a.name, b.name)),
    databases: snapshot.databases.filter(db => databaseIds.has(db.id)).sort((a, b) => Number(directIds.has(b.id)) - Number(directIds.has(a.id)) || naturalCompare(a.name, b.name)),
    forests: snapshot.forests.filter(forest => forestIds.has(forest.id)).sort((a, b) => naturalCompare(index.symbols.get(a.id)?.label ?? a.name, index.symbols.get(b.id)?.label ?? b.name)),
    hosts: snapshot.hosts.filter(host => hostIds.has(host.id)).sort((a, b) => naturalCompare(a.name, b.name)),
    directIds, warnings,
  };
}
export type TopologyProjection = ReturnType<typeof projectTopology>;
