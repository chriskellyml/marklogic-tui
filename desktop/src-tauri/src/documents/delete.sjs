// All deletes are part of one update transaction, so an error rolls back the batch.
declareUpdate();
let count = 0;
if (input.folder) {
  for (const value of cts.uris(input.uri, ['document', 'item-order', 'ascending'])) {
    const uri = String(value);
    if (!uri.startsWith(input.uri)) break;
    xdmp.documentDelete(uri);
    count++;
  }
} else {
  if (!fn.docAvailable(input.uri)) throw new Error('Document no longer exists or is not readable: ' + input.uri);
  xdmp.documentDelete(input.uri);
  count = 1;
}
JSON.stringify({deleted: count});
