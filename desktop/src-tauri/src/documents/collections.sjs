const values = Array.from(cts.collections(input.after || input.prefix,
  ['document', 'item-order', 'ascending', 'limit=102']), String)
  .filter(value => value !== input.after && value.startsWith(input.prefix));
JSON.stringify({collections: values.slice(0, 100), next: values.length > 100 ? values[99] : null});
