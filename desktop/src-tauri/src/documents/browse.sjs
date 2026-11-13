// Seek past each branch in the URI lexicon instead of materializing its descendants.
const queries = input.filter.collections.map(uri => cts.collectionQuery(uri));
const query = input.filter.mode === 'all' ? cts.andQuery(queries) :
  queries.length ? cts.orQuery(queries) : cts.trueQuery();
let cursor = input.cursor || {start: input.prefix, exclusive: false};
const entries = [];
const continuations = [];
while (entries.length < 101) {
  const candidates = Array.from(cts.uris(cursor.start,
    ['document', 'item-order', 'ascending', 'limit=2'], query), String);
  const uri = candidates.find(value => !cursor.exclusive || value !== cursor.start);
  if (uri === undefined || !uri.startsWith(input.prefix)) break;
  // At the root, /a/ and a/ are distinct paths; omit the empty leading segment.
  const from = input.prefix.length || (uri.startsWith('/') ? 1 : 0);
  const slash = uri.indexOf('/', from);
  if (slash >= 0) {
    const path = uri.slice(0, slash + 1);
    entries.push({uri: path, name: path.slice(input.prefix.length), kind: 'folder'});
    // '/' is immediately followed by '0' in codepoint order. This skips the
    // entire branch while preserving neighboring names such as /a0.json.
    cursor = {start: path.slice(0, -1) + '0', exclusive: false};
  } else {
    entries.push({uri, name: uri.slice(input.prefix.length) || '(document at this path)', kind: 'document'});
    cursor = {start: uri, exclusive: true};
  }
  continuations.push(cursor);
}
JSON.stringify({entries: entries.slice(0, 100), nextCursor: entries.length > 100 ? continuations[99] : null});
