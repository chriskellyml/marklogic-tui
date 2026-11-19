import { CompletionContext, startCompletion } from '@codemirror/autocomplete';
import { EditorView } from '@codemirror/view';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterAll, beforeAll, expect, it, vi } from 'vitest';
import { QueryEditor } from './QueryEditor';
import type { Language } from './api';
import { marklogicJavascriptCompletion, xqueryCompletion } from './query-completion';

// jsdom has no text layout. CodeMirror still renders and updates the completion list.
beforeAll(() => {
  Object.defineProperty(Range.prototype, 'getClientRects', { configurable: true, value: () => [] });
});
afterAll(() => {
  Reflect.deleteProperty(Range.prototype, 'getClientRects');
});

function openEditor(source: string, language: Language, disabled = false) {
  const onChange = vi.fn();
  const { container } = render(
    <QueryEditor source={source} language={language} onChange={onChange} disabled={disabled} />,
  );
  const view = EditorView.findFromDOM(container.querySelector<HTMLElement>('.cm-editor')!)!;
  return { view, onChange };
}

function findOption(label: string) {
  return screen.findByText((_, element) =>
    element?.classList.contains('cm-completionLabel') === true && element.textContent === label,
  );
}

it('suggests XQuery function names while typing and applies a selection', async () => {
  const source = 'cts:word-';
  const { view, onChange } = openEditor(source, 'xquery');
  act(() => view.dispatch({
    changes: { from: source.length, insert: 'q' },
    selection: { anchor: source.length + 1 },
    userEvent: 'input.type',
  }));

  fireEvent.mouseDown(await findOption('cts:word-query'));
  expect(onChange.mock.lastCall?.[0]).toBe('cts:word-query');
});

it('completes a JavaScript member on Ctrl+Space and preserves its existing arguments', async () => {
  const prefix = 'xdmp.documentG';
  const { view, onChange } = openEditor(`${prefix}e("/a.xml")`, 'javascript');
  act(() => view.dispatch({ selection: { anchor: prefix.length } }));
  fireEvent.keyDown(view.contentDOM, { key: ' ', code: 'Space', ctrlKey: true });

  fireEvent.mouseDown(await findOption('documentGet'));
  expect(onChange.mock.lastCall?.[0]).toBe('xdmp.documentGet("/a.xml")');
});

it.each([
  ['xquery', 'let $document := fn:doc("/a.xml") return $doc', '$document'],
  ['javascript', 'const documentUri = "/a.xml"; docum', 'documentUri'],
] as const)('suggests a %s variable already in the query', async (language, source, variable) => {
  const { view } = openEditor(source, language);
  act(() => {
    view.dispatch({ selection: { anchor: source.length } });
    startCompletion(view);
  });
  expect(await findOption(variable)).toBeInTheDocument();
});

it.each([
  ['xquery', '(: xdmp:doc', xqueryCompletion],
  ['xquery', '"xdmp:doc', xqueryCompletion],
  ['javascript', '// xdmp.doc', marklogicJavascriptCompletion],
  ['javascript', 'other.xdmp.doc', marklogicJavascriptCompletion],
] as const)('ignores %s fragments outside the matching API context (%s)', (language, source, complete) => {
  const { view } = openEditor(source, language);
  expect(complete(new CompletionContext(view.state, source.length, true))).toBeNull();
});

it.each([
  ['text', false],
  ['javascript', true],
] as const)('keeps completion off for plain text and locked editors (%s, %s)', (language, disabled) => {
  const { view } = openEditor('', language, disabled);
  expect(startCompletion(view)).toBe(false);
});
