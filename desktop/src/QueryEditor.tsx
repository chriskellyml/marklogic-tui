import CodeMirror from '@uiw/react-codemirror';
import { javascript } from '@codemirror/lang-javascript';
import { HighlightStyle, StreamLanguage, syntaxHighlighting } from '@codemirror/language';
import { tags } from '@lezer/highlight';
import { xQuery } from '@codemirror/legacy-modes/mode/xquery';
import { EditorView } from '@codemirror/view';
import { memo, useMemo } from 'react';
import type { Language } from './api';

const languages = { javascript: javascript(), xquery: StreamLanguage.define(xQuery), text: [] };
const highlighting = HighlightStyle.define([
  { tag: tags.comment, color: '#8c99ab', fontStyle: 'italic' },
  { tag: tags.keyword, color: '#bfa5df' },
  { tag: tags.string, color: '#a7cfa9' },
  { tag: tags.number, color: '#dec58d' },
  { tag: tags.function(tags.variableName), color: '#9fc8df' },
  { tag: tags.operator, color: '#c5b6db' },
  { tag: tags.tagName, color: '#a9d6c3' },
]);
const editorTheme = EditorView.theme({
  '&': { background: '#151a20', color: '#dce3ec', height: '100%', fontSize: '12px' },
  '.cm-scroller': { fontFamily: "'SFMono-Regular', Consolas, 'Liberation Mono', monospace", lineHeight: '1.55', overflow: 'auto' },
  '.cm-content': { padding: '8px 0', caretColor: '#9dd7c3' },
  '.cm-line': { padding: '0 12px' },
  '.cm-gutters': { background: '#151a20', color: '#626f80', border: 'none', padding: '0 0 0 12px' },
  '.cm-activeLineGutter, .cm-activeLine': { background: '#ffffff04' },
  '&.cm-focused': { outline: 'none' },
  '.cm-selectionBackground, &.cm-focused .cm-selectionBackground': { background: '#355b59 !important' },
}, { dark: true });

export const QueryEditor = memo(function QueryEditor({ source, language, onChange, disabled, label = 'Query source' }: {
  source: string; language: Language; onChange: (source: string) => void; disabled: boolean; label?: string;
}) {
  const extensions = useMemo(() => [languages[language], syntaxHighlighting(highlighting)], [language]);
  return <CodeMirror value={source} theme={editorTheme} height="100%" editable={!disabled}
    aria-label={label} extensions={extensions}
    basicSetup={{ foldGutter: true, highlightActiveLine: true }} onChange={onChange} />;
});
