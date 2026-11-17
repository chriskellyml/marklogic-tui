import { expect, it } from 'vitest';
import example from './fixtures/rewriter-example.xml?raw';
import { parseRewriter, routeShape } from './rewriter';
import { buildRestRequest } from './rest-api';

it('derives public routes and methods from the supplied rewriter without copying server-injected params', () => {
  const routes = parseRewriter(example);
  const documents = routes.filter(route => route.pattern === '^/(v1|LATEST)/api/document$');
  expect(documents.map(route => route.method)).toEqual(['GET', 'POST', 'PUT', 'DELETE']);
  expect(documents[0].draft?.params).toEqual([]);
  expect(documents[0].target).toBe('/ext/httpServices/api/endpoint.xqy');
  expect(buildRestRequest(documents[0].draft!).path).toBe('/v1/api/document');
  const captured = routes.find(route => route.pattern === '^/(v1|LATEST)/DESCRIBE/(.+)$')!;
  expect(captured.draft?.pathParams).toHaveLength(2);
  expect(() => buildRestRequest(captured.draft!)).toThrow('required');
  captured.draft!.pathParams![1].value = 'example';
  expect(buildRestRequest(captured.draft!).path).toBe('/v1/DESCRIBE/example');
  expect(routes.some(route => route.path === '/invoke')).toBe(true);
});

it('inherits nested method and header conditions without treating sibling optional branches as required', () => {
  const routes = parseRewriter(`<rewriter xmlns="http://marklogic.com/xdmp/rewriter"><match-method any-of="GET POST"><match-path matches="^/items/([0-9]+)$"><match-method any-of="POST PUT"><match-content-type any-of="application/json"><match-query-param name="mode" value="test"><dispatch>/write.xqy</dispatch></match-query-param></match-content-type></match-method><dispatch>/read.xqy</dispatch></match-path></match-method></rewriter>`);
  const write = routes.find(route => route.target === '/write.xqy')!;
  expect(write.method).toBe('POST');
  expect(write.draft?.headers[0].value).toBe('application/json');
  expect(write.draft?.params[0].value).toBe('test');
  expect(routes.filter(route => route.target === '/read.xqy').map(route => route.method)).toEqual(['GET', 'POST']);
  expect(routes.find(route => route.target === '/read.xqy')!.draft?.params).toEqual([]);
});

it('handles legacy REST options, defaults and invalid or unsupported regex shapes', () => {
  const routes = parseRewriter(`<rest:options xmlns:rest="http://marklogic.com/appservices/rest"><rest:request uri="^/search$" endpoint="/search.xqy"><rest:http method="GET"><rest:param name="q" required="true"/><rest:param name="limit" default="10"/></rest:http><rest:uri-param name="internal">$1</rest:uri-param></rest:request></rest:options>`);
  expect(routes[0].draft?.params).toHaveLength(2);
  expect(routes[0].draft?.params[1].value).toBe('10');
  expect(() => parseRewriter('<rewriter>')).toThrow('valid XML');
  expect(() => routeShape('^/(one/(two))$')).toThrow('Nested');
  expect(routeShape('^/items/?$').path).toBe('/items');
});
