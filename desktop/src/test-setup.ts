import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

afterEach(cleanup);

// Keep browser storage isolated from Node's experimental, file-backed Web Storage.
// Vitest 3 can inherit Node 26's undefined localStorage instead of jsdom's getter.
const storage = new Map<string, string>();
Object.defineProperty(window, 'localStorage', { configurable: true, value: {
  get length() { return storage.size; },
  getItem: (key: string) => storage.get(key) ?? null,
  setItem: (key: string, value: string) => { storage.set(key, String(value)); },
  removeItem: (key: string) => { storage.delete(key); },
  clear: () => storage.clear(),
  key: (index: number) => [...storage.keys()][index] ?? null,
} satisfies Storage });
