import { describe, expect, it } from 'vitest';
import { formatLogDetail } from './log-detail';

describe('expanded log formatting', () => {
  it('formats JSON after the log prefix while preserving case and values', () => {
    expect(formatLogDetail('2026-11-12 10:00:00.123 Critical: {"foo":{"x":1},"s":"Hi"}')).toBe(
      '2026-11-12 10:00:00.123 Critical: {\n  "foo": {\n    "x": 1\n  },\n  "s": "Hi"\n}');
  });
  it('formats XML elements but preserves mixed content', () => {
    expect(formatLogDetail('2026-11-12 10:00:00 Error: <root><a>value</a><b><c/></b></root>')).toBe(
      '2026-11-12 10:00:00 Error: <root>\n  <a>value</a>\n  <b>\n    <c/>\n  </b>\n</root>');
    const mixed = '2026-11-12 10:00:00 Error: <root>Hello <b>world</b>!</root>';
    expect(formatLogDetail(mixed)).toBe(mixed);
  });
  it('leaves malformed JSON, XML and ordinary text unchanged', () => {
    for (const raw of ['2026-11-12 10:00:00 Error: {bad}', '2026-11-12 10:00:00 Error: <broken>', 'plain <not xml>']) {
      expect(formatLogDetail(raw)).toBe(raw);
    }
  });
});
