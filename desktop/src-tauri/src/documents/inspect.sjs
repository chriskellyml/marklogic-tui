const uri = input.uri;
if (!fn.docAvailable(uri)) throw new Error('Document no longer exists or is not readable: ' + uri);
const doc = cts.doc(uri);
const format = doc.documentFormat.toLowerCase();
let content = format === 'binary' ? '' : format === 'text' ? String(fn.string(doc)) : xdmp.quote(doc, {indent: 'yes'});
const warnings = [];
if (content.length > 8 * 1024 * 1024) {
  content = '';
  warnings.push('Content exceeds the 8 MiB viewer limit. Metadata is still available.');
}
function optional(label, read, fallback) {
  try { return read(); } catch (error) { warnings.push(label + ': ' + error.message); return fallback; }
}
const result = {
  uri, format, content,
  collections: optional('Collections', () => Array.from(xdmp.documentGetCollections(uri), String), []),
  metadata: optional('Metadata', () => xdmp.documentGetMetadata(uri) || {}, {}),
  permissions: optional('Permissions', () => xdmp.documentGetPermissions(uri, 'objects'), []),
  quality: optional('Quality', () => Number(xdmp.documentGetQuality(uri)), null),
  timestamp: optional('Timestamp', () => String(xdmp.documentTimestamp(uri)), null),
  properties: optional('Properties', () => Array.from(xdmp.documentGetProperties(uri), node => xdmp.quote(node, {indent: 'yes'})).join('\n'), ''),
  warnings
};
JSON.stringify(result);
