import { invoke } from '@tauri-apps/api/core';
import type { Connection } from './api';

export interface CollectionFilter { collections: string[]; mode: 'any' | 'all' }
export interface CollectionSet extends CollectionFilter { name: string }
export interface UriCursor { start: string; exclusive: boolean }
export interface DocumentEntry { uri: string; name: string; kind: 'folder' | 'document' }
export interface DocumentPage { entries: DocumentEntry[]; nextCursor: UriCursor | null }
export interface CollectionPage { collections: string[]; next: string | null }
export interface DocumentDetail {
  uri: string; format: 'xml' | 'json' | 'text' | 'binary'; content: string;
  collections: string[]; metadata: Record<string, unknown>; permissions: unknown[];
  quality: number | null; timestamp: string | null; properties: string; warnings: string[];
}

export const documentsApi = {
  browse: (connection: Connection, prefix: string, cursor: UriCursor | null, filter: CollectionFilter) =>
    invoke<DocumentPage>('browse_documents', { connection, prefix, cursor, filter }),
  collections: (connection: Connection, prefix: string, after: string | null) =>
    invoke<CollectionPage>('browse_collections', { connection, prefix, after }),
  inspect: (connection: Connection, uri: string) => invoke<DocumentDetail>('inspect_document', { connection, uri }),
};

export function readCollectionSets(key: string): CollectionSet[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(key) ?? '[]');
    return Array.isArray(value) ? value.filter((set): set is CollectionSet => set &&
      typeof set.name === 'string' && (set.mode === 'any' || set.mode === 'all') &&
      Array.isArray(set.collections) && set.collections.length <= 100 && set.collections.every((item: unknown) => typeof item === 'string')) : [];
  } catch { return []; }
}
