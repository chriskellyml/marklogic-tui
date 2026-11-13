import { EditorState } from '@codemirror/state';
import { expect, it } from 'vitest';
import { indentationFold } from './document-folding';

it('folds XML children while leaving closing tags and following siblings visible', () => {
  const state = EditorState.create({ doc: '<root>\n  <child>\n    value\n  </child>\n</root>\n<sibling/>' });
  expect(indentationFold(state, 0)).toEqual({ from: state.doc.line(1).to, to: state.doc.line(4).to });
  expect(indentationFold(state, state.doc.line(2).from)).toEqual({ from: state.doc.line(2).to, to: state.doc.line(3).to });
  expect(indentationFold(state, state.doc.line(5).from)).toBeNull();
});
