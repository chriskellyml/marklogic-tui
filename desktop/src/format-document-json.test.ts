import { expect, it } from 'vitest';
import { formatDocumentJson } from './format-document-json';

it('indents nested JSON without changing large numbers, escapes, or empty containers', () => {
  const input = '{"id":9223372036854775807,"text":"a\\\"{b}\\n","items":[{},[],true]}';
  expect(formatDocumentJson(input)).toBe('{\n  "id": 9223372036854775807,\n  "text": "a\\\"{b}\\n",\n  "items": [\n    {},\n    [],\n    true\n  ]\n}');
});

it('keeps unparseable content visible unchanged', () => {
  expect(formatDocumentJson('{"broken":')).toBe('{"broken":');
});
