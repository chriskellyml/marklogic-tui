import { ifNotIn, insertCompletionText, pickedCompletion } from '@codemirror/autocomplete';
import type { Completion } from '@codemirror/autocomplete';
import { syntaxTree } from '@codemirror/language';
import type { EditorView } from '@codemirror/view';
import catalog from './marklogic-functions.json';

const xqueryNamePart = /[\p{L}\p{M}\p{N}_.-]+/u;
const javascriptNamePart = /[\p{ID_Continue}$\u200c\u200d]+/u;
const xqueryName = /^[\p{L}_][\p{L}\p{M}\p{N}_.-]*$/u;
const javascriptName = /^[\p{ID_Start}$_][\p{ID_Continue}$\u200c\u200d]*$/u;
const xqueryVariable = /^\$[\p{L}_][\p{L}\p{M}\p{N}_.-]*(?::[\p{L}_][\p{L}\p{M}\p{N}_.-]*)?$/u;

function replaceNameSuffix(namePart: RegExp) {
  const suffixPattern = new RegExp(`^${namePart.source}`, namePart.flags);
  return (view: EditorView, completion: Completion, from: number, to: number) => {
    const line = view.state.doc.lineAt(to);
    const suffix = suffixPattern.exec(line.text.slice(to - line.from));
    const end = to + (suffix?.[0].length ?? 0);
    view.dispatch({
      ...insertCompletionText(view.state, completion.label, from, end),
      annotations: pickedCompletion.of(completion),
    });
  };
}

const applyXquery = replaceNameSuffix(xqueryNamePart);
const applyJavascript = replaceNameSuffix(javascriptNamePart);

// API names come from the MarkLogic 12 indices at docs.marklogic.com/all
// and docs.marklogic.com/js/all, plus standard XML Schema constructors.
// Libraries still require their usual imports.
const xqueryFunctions = new Map(Object.entries({ ...catalog.xquery, xs: catalog.xs }).map(([prefix, names]) => [
  prefix,
  names.map((name) => ({ label: `${prefix}:${name}`, type: 'function', apply: applyXquery })),
]));
const { 'Global-Object': globals, ...javascriptNamespaces } = catalog.javascript;
const javascriptFunctions = new Map(Object.entries({ ...javascriptNamespaces, xs: catalog.xs }).map(([prefix, names]) => [
  prefix,
  names.map((label) => ({ label, type: 'function', apply: applyJavascript })),
]));

const xqueryKeywords = [
  'as', 'at', 'cast as', 'castable as', 'catch', 'declare', 'else', 'external',
  'for', 'function', 'group by', 'if', 'import module namespace', 'in', 'instance of',
  'let', 'module namespace', 'order by', 'return', 'satisfies', 'some', 'every',
  'switch', 'then', 'treat as', 'try', 'typeswitch', 'where', 'xquery version',
];
const xqueryRootOptions: Completion[] = [
  ...Array.from(xqueryFunctions.keys(), (label) => ({ label, type: 'namespace', apply: applyXquery })),
  ...catalog.xquery.fn.map((label) => ({ label, type: 'function', apply: applyXquery })),
  ...xqueryKeywords.map((label) => ({ label, type: 'keyword', apply: applyXquery })),
];
const javascriptRootOptions: Completion[] = [
  ...Array.from(javascriptFunctions.keys(), (label) => ({ label, type: 'namespace', apply: applyJavascript })),
  ...globals.map((label) => ({ label, type: 'function', apply: applyJavascript })),
];

export const xqueryCompletion = ifNotIn(['comment', 'string', 'tagName', 'attributeName'], (context) => {
  const token = context.matchBefore(/[$\p{L}\p{M}\p{N}_.:-]+/u);
  if (!token) return context.explicit ? { from: context.pos, options: xqueryRootOptions } : null;
  const { from, text } = token;

  if (text.startsWith('$')) {
    const variables = new Set<string>();
    syntaxTree(context.state).iterate({
      to: from,
      enter(node) {
        if (node.name !== 'variableName' || node.to > from) return;
        const name = context.state.sliceDoc(node.from, node.to);
        if (xqueryVariable.test(name)) variables.add(name);
      },
    });
    return {
      from,
      options: Array.from(variables, (label) => ({ label, type: 'variable', apply: applyXquery })),
      validFor: /^\$[\p{L}\p{M}\p{N}_.:-]*$/u,
    };
  }

  const colon = text.indexOf(':');
  if (colon !== -1) {
    const prefix = text.slice(0, colon);
    const options = xqueryFunctions.get(prefix);
    if (!options || text.includes(':', colon + 1)) return null;
    return {
      from,
      options,
      validFor: (value) => value.startsWith(`${prefix}:`)
        && (value.length === colon + 1 || xqueryName.test(value.slice(colon + 1))),
    };
  }

  return xqueryName.test(text) ? { from, options: xqueryRootOptions, validFor: xqueryName } : null;
});

export const marklogicJavascriptCompletion = ifNotIn([
  'LineComment', 'BlockComment', 'String', 'RegExp',
  'VariableDefinition', 'PropertyDefinition', 'Label',
], (context) => {
  const node = syntaxTree(context.state).resolveInner(context.pos, -1);
  if (node.name === 'TemplateString') return null;
  if (node.name === 'PropertyName' || node.name === '.' || node.name === '?.') {
    const receiver = node.parent?.firstChild;
    if (node.parent?.name !== 'MemberExpression' || receiver?.name !== 'VariableName') return null;
    const options = javascriptFunctions.get(context.state.sliceDoc(receiver.from, receiver.to));
    const from = node.name === 'PropertyName' ? node.from : context.pos;
    return options ? { from, options, validFor: /^([\p{ID_Start}$_][\p{ID_Continue}$\u200c\u200d]*)?$/u } : null;
  }

  const token = context.matchBefore(javascriptNamePart);
  if (token ? !javascriptName.test(token.text) : !context.explicit) return null;
  return { from: token?.from ?? context.pos, options: javascriptRootOptions, validFor: javascriptName };
});
