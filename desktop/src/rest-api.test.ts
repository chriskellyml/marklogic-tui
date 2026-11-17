import { beforeEach, describe, expect, it } from 'vitest';
import { buildRestRequest, createRestDraft, readRestDrafts, restBaseUrl, restEntry, restStorageKey } from './rest-api';

beforeEach(() => localStorage.clear());

describe('REST environment and request serialization', () => {
  it('uses the selected app server protocol and port, retaining the profile hostname', () => {
    const server = { name: 'test', endpoint: 'http://ml.example:8000', appServers: [{ name: 'api', port: 8043, secure: true }] };
    expect(restBaseUrl(server, 'api')).toBe('https://ml.example:8043');
    expect(restBaseUrl(server, null)).toBe('http://ml.example:8000');
    expect(restBaseUrl(undefined, null)).toBe('');
    expect(restBaseUrl({ ...server, endpoint: 'http://[::1]:8000' }, 'api')).toBe('https://[::1]:8043');
  });

  it('encodes document URIs, preserves repeated params and inline query, and omits disabled rows', () => {
    const draft = createRestDraft('document');
    draft.path = '/v1/documents?category=content';
    draft.params = [restEntry('uri', '/folder/a & b.json'), restEntry('collection', 'one'), restEntry('collection', 'two'), { ...restEntry('ignored', 'value'), enabled: false }];
    draft.headers.push({ ...restEntry('X-Disabled', 'no'), enabled: false });
    const request = buildRestRequest(draft);
    expect(request.path).toBe('/v1/documents?category=content&uri=%2Ffolder%2Fa+%26+b.json&collection=one&collection=two');
    expect(request.headers).toEqual([['Accept', 'application/json']]);
    expect(request.body).toBeNull();
  });

  it('rejects URLs that could bypass the selected environment', () => {
    for (const path of ['https://other.example/v1/search', '//other.example/v1/search', '/\\other.example', '/v1/search#fragment']) {
      expect(() => buildRestRequest({ ...createRestDraft(), path })).toThrow('relative path');
    }
  });

  it('restores saved requests and rejects invalid stored data', () => {
    const draft = createRestDraft('structured');
    localStorage.setItem(restStorageKey, JSON.stringify([draft]));
    expect(readRestDrafts()).toEqual([draft]);
    expect(buildRestRequest(draft).body).toBe(draft.body);
    localStorage.setItem(restStorageKey, JSON.stringify([{ ...draft, method: 'INVALID' }]));
    expect(readRestDrafts()[0].method).toBe('GET');
    localStorage.setItem(restStorageKey, '[]');
    expect(readRestDrafts()).toEqual([]);
  });
});
