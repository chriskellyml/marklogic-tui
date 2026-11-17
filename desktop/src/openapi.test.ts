import { expect, it } from 'vitest';
import { parseOpenApi } from './openapi';
import { buildRestRequest } from './rest-api';

it('generates operations with referenced bodies, overrides parameters and keeps only the server path', () => {
  const parsed = parseOpenApi({ openapi: '3.1.0', info: { title: 'Widgets' }, servers: [{ url: 'https://production.example/api/{version}', variables: { version: { default: 'v2' } } }], components: { schemas: { Widget: { type: 'object', properties: { name: { type: 'string', default: 'test' }, id: { type: 'string', readOnly: true } } } }, parameters: { id: { in: 'path', name: 'id', required: true, example: 'a/b' } } }, paths: { '/widgets/{id}': { parameters: [{ $ref: '#/components/parameters/id' }, { in: 'query', name: 'q', example: 'old' }], post: { tags: ['Widgets'], parameters: [{ in: 'query', name: 'q', example: 'new' }, { in: 'query', name: 'tags', schema: { type: 'array', default: ['a', 'b'] } }, { in: 'header', name: 'Authorization', example: 'secret' }], requestBody: { content: { 'application/json': { schema: { $ref: '#/components/schemas/Widget' } } } } } } } });
  const draft = parsed.operations[0].draft!;
  expect(parsed.title).toBe('Widgets');
  expect(buildRestRequest(draft)).toMatchObject({ path: '/api/v2/widgets/a%2Fb?q=new&tags=a&tags=b', body: '{\n  "name": "test"\n}' });
  expect(draft.headers.some(h => h.name === 'Authorization')).toBe(false);
});

it('rejects unresolved path parameters and reports unsupported refs instead of silently omitting them', () => {
  const spec = { openapi: '3.0.3', paths: { '/items/{id}': { get: { parameters: [{ name: 'id', in: 'path', required: true, schema: { type: 'string' } }] }, post: { requestBody: { $ref: './body.yml' } } } } };
  const parsed = parseOpenApi(spec);
  expect(() => buildRestRequest(parsed.operations.find(op => op.method === 'GET')!.draft!)).toThrow('required');
  expect(parsed.operations.find(op => op.method === 'POST')!.error).toContain('External reference');
});

it('supports Swagger basePath, body schemas and operation-level server overrides', () => {
  const swagger = parseOpenApi({ swagger: '2.0', host: 'ignore.example', basePath: '/v1', paths: { '/search': { post: { parameters: [{ in: 'body', name: 'body', schema: { type: 'object', properties: { q: { default: 'hello' } } } }] } } } });
  expect(swagger.operations[0].draft).toMatchObject({ path: '/v1/search', bodyEnabled: true });
  const spec = parseOpenApi({ openapi: '3.0.0', servers: [{ url: 'https://ignore/global' }], paths: { '/x': { servers: [{ url: '/path' }], get: { servers: [{ url: 'https://ignore/operation' }] } } } });
  expect(spec.operations[0].draft?.path).toBe('/operation/x');
});
