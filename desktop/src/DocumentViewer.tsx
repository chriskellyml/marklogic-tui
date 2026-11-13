import { useMemo, useRef, useState } from 'react';
import CodeMirror from '@uiw/react-codemirror';
import { javascript } from '@codemirror/lang-javascript';
import { xml } from '@codemirror/legacy-modes/mode/xml';
import { foldAll, unfoldAll, foldService, StreamLanguage, HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { tags } from '@lezer/highlight';
import { EditorView } from '@codemirror/view';
import type { DocumentDetail } from './documents-api';
import { indentationFold } from './document-folding';

const theme = EditorView.theme({
  '&': { height: '100%', backgroundColor: '#151a20', color: '#dce3ec', fontSize: '12px' },
  '.cm-scroller': { overflow: 'auto', fontFamily: "'SFMono-Regular', Consolas, monospace", lineHeight: '1.6' },
  '.cm-gutters': { backgroundColor: '#151a20', color: '#718195', borderRight: '1px solid #ffffff10' },
  '&.cm-focused': { outline: 'none' },
}, { dark: true });
const indentation = foldService.of(indentationFold);
const languages = { json: javascript(), xml: [StreamLanguage.define(xml), indentation], text: indentation };
const highlighting = syntaxHighlighting(HighlightStyle.define([
  { tag: tags.string, color: '#a7cfa9' },
  { tag: tags.propertyName, color: '#a9cfe3' },
  { tag: tags.number, color: '#dec58d' },
  { tag: tags.bool, color: '#bfa5df' },
  { tag: tags.null, color: '#bfa5df' },
  { tag: tags.tagName, color: '#a9d6c3' },
  { tag: tags.attributeName, color: '#a9cfe3' },
  { tag: tags.comment, color: '#8c99ab', fontStyle: 'italic' },
]));

export function DocumentViewer({ document }: { document: DocumentDetail }) {
  const [tab, setTab] = useState<'content' | 'metadata' | 'properties'>('content');
  const [wrap, setWrap] = useState(false);
  const editor = useRef<EditorView | null>(null);
  const format = tab === 'metadata' ? 'json' : tab === 'properties' ? 'xml' : document.format === 'binary' ? 'text' : document.format;
  const source = useMemo(() => tab === 'metadata' ? JSON.stringify({
    uri: document.uri, collections: document.collections, metadata: document.metadata,
    permissions: document.permissions, quality: document.quality, timestamp: document.timestamp,
  }, null, 2) : tab === 'properties' ? document.properties : document.content, [document, tab]);
  const extensions = useMemo(() => [languages[format], highlighting, ...(wrap ? [EditorView.lineWrapping] : [])], [format, wrap]);
  return <>
    <div className="document-viewer-heading"><strong title={document.uri}>{document.uri}</strong><span>{document.format.toUpperCase()} · Read only</span></div>
    <div className="document-collections" aria-label="Document collections"><span>Collections</span>{document.collections.length ? document.collections.map(uri => <code key={uri} title={uri}>{uri}</code>) : <span>None</span>}</div>
    <div className="document-viewer-toolbar">
      <nav aria-label="Document views">{(['content', 'metadata', 'properties'] as const).map(value => <button key={value} aria-pressed={tab === value} onClick={() => setTab(value)}>{value[0].toUpperCase() + value.slice(1)}</button>)}</nav>
      <div><button onClick={() => { if (editor.current) foldAll(editor.current); }}>Fold all</button><button onClick={() => { if (editor.current) unfoldAll(editor.current); }}>Unfold all</button><button aria-pressed={wrap} onClick={() => setWrap(!wrap)}>Wrap lines</button></div>
    </div>
    {document.warnings.map(warning => <p className="document-warning" key={warning}>{warning}</p>)}
    {document.format === 'binary' && tab === 'content' ? <p className="document-empty">Binary content cannot be displayed. Inspect its metadata and properties above.</p> :
      tab === 'properties' && !source ? <p className="document-empty">This document has no properties.</p> :
        <div className="document-code" aria-label={`${tab} viewer`}><CodeMirror key={tab} value={source} height="100%" theme={theme} editable={false} readOnly extensions={extensions}
          basicSetup={{ foldGutter: true, lineNumbers: true, highlightActiveLine: false }} onCreateEditor={view => { editor.current = view; }} /></div>}
  </>;
}
