import type { EditorState } from '@codemirror/state';

/** Fold indented XML and text blocks, leaving the closing tag visible. */
export function indentationFold(state: EditorState, from: number): { from: number; to: number } | null {
  const line = state.doc.lineAt(from);
  const indent = line.text.search(/\S/);
  if (indent < 0) return null;
  let end = line.to;
  for (let number = line.number + 1; number <= state.doc.lines; number++) {
    const next = state.doc.line(number);
    const nextIndent = next.text.search(/\S/);
    if (nextIndent < 0) continue;
    if (nextIndent <= indent) break;
    end = next.to;
  }
  return end > line.to ? { from: line.to, to: end } : null;
}
