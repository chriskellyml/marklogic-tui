import { describe, expect, it } from 'vitest';
import { delimitedRows, prettyResults } from './result-formats';
describe('query result formats',()=>{
  it('parses quoted delimiters, escaped quotes and multiple lines',()=>{
    expect(delimitedRows(['one,"two, three"\n"a ""quote""",b'],',')).toEqual([['one','two, three'],['a "quote"','b']]);
    expect(delimitedRows(['a|b\nc|d'],'|')).toEqual([['a','b'],['c','d']]);
  });
  it('formats JSON and preserves non-JSON text',()=>{
    expect(prettyResults(['{"a":1}','2'])).toContain('"a": 1');
    expect(prettyResults(['{"a":1}','plain'])).toContain('plain');
  });
});
