import { useMemo, useRef } from 'react';
import CodeMirror from '@uiw/react-codemirror';
import { javascript } from '@codemirror/lang-javascript';
import { xml } from '@codemirror/legacy-modes/mode/xml';
import { foldAll, unfoldAll, foldService, StreamLanguage, HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorView } from '@codemirror/view';
import { tags } from '@lezer/highlight';
import { indentationFold } from './document-folding';

const theme = EditorView.theme({
  '&': { height: '100%', backgroundColor: '#151a20', color: '#dce3ec', fontSize: '12px' },
  '.cm-scroller': { overflow: 'auto', fontFamily: "'SFMono-Regular', Consolas, monospace", lineHeight: '1.6' },
  '.cm-gutters': { backgroundColor: '#151a20', color: '#718195', borderRight: '1px solid #ffffff10' },
  '&.cm-focused': { outline: 'none' },
}, { dark: true });
const highlight = syntaxHighlighting(HighlightStyle.define([
  { tag: tags.string, color: '#a7cfa9' }, { tag: tags.propertyName, color: '#a9cfe3' },
  { tag: tags.number, color: '#dec58d' }, { tag: [tags.bool, tags.null], color: '#bfa5df' },
  { tag: tags.tagName, color: '#a9d6c3' }, { tag: tags.attributeName, color: '#a9cfe3' },
  { tag: tags.comment, color: '#8c99ab', fontStyle: 'italic' },
]));
const languages = { json: javascript(), xml: [StreamLanguage.define(xml), foldService.of(indentationFold)], text: [] };

export function RestResponseViewer({ source, format }: { source: string; format: 'json' | 'xml' | 'text' }) {
  const editor = useRef<EditorView | null>(null);
  const extensions = useMemo(() => [languages[format], highlight], [format]);
  return <div className="rest-response-viewer">
    <div className="rest-response-viewer-toolbar"><span>{format.toUpperCase()} · Read only</span><button type="button" onClick={() => { if (editor.current) foldAll(editor.current); }}>Fold all</button><button type="button" onClick={() => { if (editor.current) unfoldAll(editor.current); }}>Unfold all</button></div>
    <div className="rest-response-code" aria-label="Response body"><CodeMirror value={source} height="100%" theme={theme} editable={false} readOnly extensions={extensions} basicSetup={{ foldGutter: true, lineNumbers: true, highlightActiveLine: false }} onCreateEditor={view => { editor.current = view; }}/></div>
  </div>;
}
