export const FILTER_FIELDS = { any: 'Any field', text: 'Record text', host: 'Host', file: 'Filename', port: 'Port', method: 'HTTP method', level: 'Level', kind: 'Log type' } as const;
export type FilterField = keyof typeof FILTER_FIELDS;
export type FilterAction = 'include' | 'exclude';
export interface FilterRule {
  id: string; text: string; enabled: boolean; action: FilterAction; field: FilterField;
  mode: 'literal' | 'exact' | 'regex'; case: 'insensitive' | 'sensitive' | 'smart';
}
export interface FilterList { id: string; name: string; rules: FilterRule[] }
export const FILTER_LISTS_KEY = 'marklogic.log-filter-lists.v1';
export function newRule(text: string, action: FilterAction, options: Partial<FilterRule> = {}): FilterRule {
  return { id: crypto.randomUUID(), text, action, enabled: true, field: 'any', mode: 'literal', case: 'insensitive', ...options };
}
// Newlines delimit bulk entries; commas, pipes and regex punctuation stay literal.
export function bulkRules(text: string, action: FilterAction, options: Partial<FilterRule> = {}) {
  return text.split(/\r?\n/).filter((term) => term.trim().length > 0).map((term) => newRule(term, action, options));
}
export function appendRules(current: FilterRule[], incoming: FilterRule[]) {
  const key = (rule: FilterRule) => JSON.stringify([rule.text, rule.action, rule.field, rule.mode, rule.case]);
  const known = new Map(current.map((rule, index) => [key(rule), index]));
  const result = [...current];
  for (const rule of incoming) {
    const index = known.get(key(rule));
    if (index !== undefined) result[index] = { ...result[index], enabled: rule.enabled };
    else { known.set(key(rule), result.length); result.push({ ...rule, id: crypto.randomUUID() }); }
  }
  return result;
}
export function readFilterLists(): FilterList[] {
  const value = window.localStorage.getItem(FILTER_LISTS_KEY);
  if (!value) return [];
  const parsed: unknown = JSON.parse(value);
  if (!parsed || typeof parsed !== 'object' || !('version' in parsed) || parsed.version !== 1 || !('lists' in parsed) || !Array.isArray(parsed.lists)) throw new Error('Unsupported saved filter lists.');
  const isRule = (rule: unknown): rule is FilterRule => {
    if (!rule || typeof rule !== 'object') return false;
    const r = rule as FilterRule;
    return typeof r.id === 'string' && typeof r.text === 'string' && typeof r.enabled === 'boolean' &&
      ['include', 'exclude'].includes(r.action) && Object.hasOwn(FILTER_FIELDS, r.field) &&
      ['literal', 'exact', 'regex'].includes(r.mode) && ['insensitive', 'sensitive', 'smart'].includes(r.case);
  };
  if (!parsed.lists.every((list) => list && typeof list.id === 'string' && typeof list.name === 'string' && Array.isArray(list.rules) && list.rules.every(isRule))) throw new Error('Saved filter lists are damaged.');
  return parsed.lists;
}
