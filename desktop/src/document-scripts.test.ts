import browse from '../src-tauri/src/documents/browse.sjs?raw';
import collections from '../src-tauri/src/documents/collections.sjs?raw';
import inspect from '../src-tauri/src/documents/inspect.sjs?raw';
import { describe, expect, it } from 'vitest';

// Run the actual server scripts against a codepoint-ordered lexicon and collection index.
function run(source: string, input: unknown, documents: { uri: string; collections: string[] }[]) {
  type Query = (document: typeof documents[number]) => boolean;
  let calls = 0;
  const cts = {
    collectionQuery: (uri: string): Query => document => document.collections.includes(uri),
    andQuery: (queries: Query[]): Query => document => queries.every(query => query(document)),
    orQuery: (queries: Query[]): Query => document => queries.some(query => query(document)),
    trueQuery: (): Query => () => true,
    uris: (start: string, options: string[], query: Query) => {
      calls++;
      return documents.filter(document => document.uri >= start && query(document)).map(document => document.uri).sort().slice(0, Number(options.find(option => option.startsWith('limit='))!.slice(6)));
    },
    collections: (start: string, options: string[]) => [...new Set(documents.flatMap(document => document.collections))].sort().filter(uri => uri >= start).slice(0, Number(options.find(option => option.startsWith('limit='))!.slice(6))),
  };
  // The last expression is the eval endpoint's JSON response.
  const executable = source.replace(/JSON.stringify\(/, 'return JSON.stringify(');
  return { value: JSON.parse(new Function('input', 'cts', executable)(input, cts)), calls };
}

const filter = { collections: [], mode: 'any' };
describe('document lexicon paging', () => {
  it('pages 100 immediate children while seeking over large branches', () => {
    const documents = Array.from({ length: 205 }, (_, index) => ({ uri: `/group-${String(index).padStart(3, '0')}/child/doc.json`, collections: [] }));
    documents.push(...Array.from({ length: 5000 }, (_, index) => ({ uri: `/group-000/bulk/${index}.xml`, collections: [] })));
    const first = run(browse, { prefix: '', cursor: null, filter }, documents);
    expect(first.value.entries).toHaveLength(100);
    expect(first.value.entries[0]).toEqual({ uri: '/group-000/', name: '/group-000/', kind: 'folder' });
    expect(first.calls).toBe(101);
    const second = run(browse, { prefix: '', cursor: first.value.nextCursor, filter }, documents).value;
    expect(second.entries).toHaveLength(100);
    expect(second.entries[0].uri).toBe('/group-100/');
    const third = run(browse, { prefix: '', cursor: second.nextCursor, filter }, documents).value;
    expect(third.entries).toHaveLength(5);
    expect(third.nextCursor).toBeNull();
  });

  it('preserves documents at directory URIs, neighboring names, literal wildcards and non-slash URIs', () => {
    const documents = ['/', '/a/', '/a/file.json', '/a0.json', '/a*b/d.xml', 'urn:example:123', 'relative/file.txt'].map(uri => ({ uri, collections: [] }));
    const root = run(browse, { prefix: '', cursor: null, filter }, documents).value;
    expect(root.entries.map((entry: { uri: string }) => entry.uri)).toEqual(['/', '/a*b/', '/a/', '/a0.json', 'relative/', 'urn:example:123']);
    const children = run(browse, { prefix: '/a/', cursor: null, filter }, documents).value;
    expect(children.entries).toEqual([{ uri: '/a/', name: '(document at this path)', kind: 'document' }, { uri: '/a/file.json', name: 'file.json', kind: 'document' }]);
  });

  it('pages leaf documents without repeating or skipping the boundary URI', () => {
    const documents = Array.from({ length: 101 }, (_, index) => ({ uri: `/a/${String(index).padStart(3, '0')}.json`, collections: [] }));
    const first = run(browse, { prefix: '/a/', cursor: null, filter }, documents).value;
    const second = run(browse, { prefix: '/a/', cursor: first.nextCursor, filter }, documents).value;
    expect(second.entries.map((entry: { uri: string }) => entry.uri)).toEqual(['/a/100.json']);
    expect(second.nextCursor).toBeNull();
  });

  it('applies OR and AND collection sets before deriving URI branches', () => {
    const documents = [ { uri: '/one/a.json', collections: ['a'] }, { uri: '/two/b.xml', collections: ['b'] }, { uri: '/both/c.txt', collections: ['a', 'b'] } ];
    const query = (mode: string) => run(browse, { prefix: '', cursor: null, filter: { mode, collections: ['a', 'b'] } }, documents).value.entries.map((entry: { uri: string }) => entry.uri);
    expect(query('any')).toEqual(['/both/', '/one/', '/two/']);
    expect(query('all')).toEqual(['/both/']);
    expect(run(browse, { prefix: '', cursor: null, filter: { mode: 'all', collections: [] } }, documents).value.entries).toHaveLength(3);
  });

  it('pages collection names and respects literal prefixes', () => {
    const documents = [{ uri: '/doc', collections: Array.from({ length: 102 }, (_, index) => `set/${String(index).padStart(3, '0')}`) }];
    const first = run(collections, { prefix: 'set/', after: null }, documents).value;
    expect(first.collections).toHaveLength(100);
    expect(run(collections, { prefix: 'set/', after: first.next }, documents).value.collections).toEqual(['set/100', 'set/101']);
    expect(run(collections, { prefix: 'missing', after: null }, documents).value.collections).toEqual([]);
  });
});

describe('document inspection', () => {
  function inspectDocument(format: string, content: string, metadataAvailable = true) {
    const fn = { docAvailable: () => true, string: () => content };
    const cts = { doc: () => ({ documentFormat: format }) };
    const xdmp = {
      quote: () => content,
      documentGetCollections: () => ['orders'],
      documentGetMetadata: () => { if (!metadataAvailable) throw new Error('Permission denied'); return { source: 'import' }; },
      documentGetPermissions: () => [{ capability: 'read', roleId: '123' }],
      documentGetQuality: () => 5,
      documentTimestamp: () => '17945856000000000',
      documentGetProperties: () => [],
    };
    return JSON.parse(new Function('input', 'fn', 'cts', 'xdmp', inspect.replace(/JSON.stringify\(/, 'return JSON.stringify('))({ uri: '/document' }, fn, cts, xdmp));
  }

  it.each([['XML', '<!-- comment --><root/>'], ['JSON', '"a JSON string"'], ['TEXT', '<plain text> & unchanged\nnext line']])('uses the actual %s document format and keeps content', (format, content) => {
    const result = inspectDocument(format, content);
    expect(result.format).toBe(format.toLowerCase());
    expect(result.content).toBe(content);
    expect(result.collections).toEqual(['orders']);
    expect(result.timestamp).toBe('17945856000000000');
  });

  it('retains content and other metadata if one metadata category is unavailable', () => {
    const result = inspectDocument('JSON', '{"ok":true}', false);
    expect(result.content).toBe('{"ok":true}');
    expect(result.metadata).toEqual({});
    expect(result.quality).toBe(5);
    expect(result.warnings).toEqual(['Metadata: Permission denied']);
  });

  it('omits binary and oversized content while keeping metadata', () => {
    expect(inspectDocument('BINARY', 'binary').content).toBe('');
    const result = inspectDocument('TEXT', 'x'.repeat(8 * 1024 * 1024 + 1));
    expect(result.content).toBe('');
    expect(result.metadata).toEqual({ source: 'import' });
    expect(result.warnings[0]).toContain('viewer limit');
  });
});
