import { invoke } from '@tauri-apps/api/core';
import type { Connection } from './api';
import type { Capture } from './host-capture';

export interface TopologyGroup { id: string; name: string }
export interface TopologyHost { id: string; name: string; groupId: string }
export interface TopologyAppServer {
  id: string; name: string; groupId: string; hostIds: string[]; kind: string;
  port: number; secure: boolean; enabled: boolean; contentDatabaseId: string;
  modulesDatabaseId: string; root: string; authentication: string;
}
export interface TopologyDatabase {
  id: string; name: string; forestIds: string[]; securityDatabaseId: string;
  schemaDatabaseId: string; triggersDatabaseId: string;
}
export interface TopologyForest {
  id: string; name: string; hostId: string; replicaIds: string[];
  failoverHostIds: string[]; dataDirectory: string; enabled: boolean;
}
export interface TopologySnapshot {
  capturedAt: string; groups: TopologyGroup[]; hosts: TopologyHost[];
  appServers: TopologyAppServer[]; databases: TopologyDatabase[]; forests: TopologyForest[];
}
export const topologyApi = {
  snapshot: (connection: Connection, cached: boolean) => invoke<Capture<TopologySnapshot> | null>('topology_snapshot', { connection, cached }),
};
