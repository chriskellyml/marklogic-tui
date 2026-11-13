import { useState } from 'react';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { LogFilterDialog } from './LogFilterDialog';
import { logsApi } from './logs-api';
import { FILTER_LISTS_KEY, newRule, type FilterAction, type FilterRule } from './log-filters';
vi.mock('./logs-api', () => ({ logsApi: { validateFilterRules: vi.fn() } }));
const close = vi.fn();
function Editor({ initial = [] }: { initial?: FilterRule[] }) {
  const [rules, setRules] = useState(initial);
  const [action, setAction] = useState<FilterAction>('include');
  return <><LogFilterDialog action={action} rules={rules} onChange={setRules} onAction={setAction} onClose={close} position={null} onPosition={vi.fn()} /><output data-testid="rules">{JSON.stringify(rules)}</output></>;
}
function rules(): FilterRule[] { return JSON.parse(screen.getByTestId('rules').textContent!); }
async function add(text: string) {
  fireEvent.change(screen.getByRole('textbox', { name: /Filter (in|out) text/ }), { target: { value: text } });
  await waitFor(() => expect(screen.getByRole('button', { name: 'Add terms' })).toBeEnabled());
  await userEvent.click(screen.getByRole('button', { name: 'Add terms' }));
}
beforeEach(() => {
  vi.clearAllMocks(); window.localStorage.clear();
  vi.mocked(logsApi.validateFilterRules).mockResolvedValue(undefined);
});
describe('filter editor', () => {
  it('bulk-adds unchanged terms, scopes exact values, toggles terms and deduplicates', async () => {
    render(<Editor />);
    const input = screen.getByRole('textbox', { name: 'Filter in text' });
    expect(input).toHaveAttribute('autocapitalize', 'off');
    expect(input).toHaveAttribute('autocorrect', 'off');
    expect(input).toHaveAttribute('spellcheck', 'false');
    await add('error\nTimeout\na,b|c\n');
    expect(rules().map((r) => r.text)).toEqual(['error', 'Timeout', 'a,b|c']);
    await userEvent.click(screen.getByRole('checkbox', { name: 'Enable term 2' }));
    expect(rules()[1].enabled).toBe(false);
    await add('Timeout');
    expect(rules()).toHaveLength(3);
    expect(rules()[1].enabled).toBe(true);
    await userEvent.selectOptions(screen.getByLabelText('New term field'), 'port');
    await userEvent.selectOptions(screen.getByLabelText('New term match'), 'exact');
    await add('8000');
    expect(rules()[3]).toMatchObject({ text: '8000', field: 'port', mode: 'exact' });
    await userEvent.click(screen.getByRole('button', { name: 'Disable all' }));
    expect(rules().every((r) => !r.enabled)).toBe(true);
    await userEvent.click(screen.getByRole('button', { name: 'Enable all' }));
    expect(rules().every((r) => r.enabled)).toBe(true);
  });
  it('saves both keep/drop terms and disabled state, reloads, renames, updates, appends and deletes lists', async () => {
    const initial = [newRule('Timeout', 'include'), newRule('Noise', 'exclude', { enabled: false, mode: 'regex' })];
    const rendered = render(<Editor initial={initial} />);
    fireEvent.change(screen.getByLabelText('Filter list name'), { target: { value: 'Production' } });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save as new' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Save as new' }));
    expect(JSON.parse(window.localStorage.getItem(FILTER_LISTS_KEY)!).lists[0].rules).toEqual(initial);
    rendered.unmount();
    render(<Editor />);
    await userEvent.selectOptions(screen.getByLabelText('Saved filter lists'), screen.getByRole('option', { name: 'Production' }).getAttribute('value')!);
    await userEvent.click(screen.getByRole('button', { name: 'Load' }));
    expect(rules()).toEqual(initial);
    fireEvent.change(screen.getByLabelText('Filter list name'), { target: { value: 'Incident' } });
    await userEvent.click(screen.getByRole('button', { name: 'Rename' }));
    expect(screen.getByRole('option', { name: 'Incident' })).toBeInTheDocument();
    await add('Another');
    await waitFor(() => expect(screen.getByRole('button', { name: 'Update list' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Update list' }));
    await userEvent.click(screen.getByRole('button', { name: 'Remove all' }));
    expect(rules()).toHaveLength(1); // Only the disabled drop rule remains.
    await userEvent.click(screen.getByRole('button', { name: 'Append' }));
    expect(rules()).toHaveLength(3);
    expect(rules().find((r) => r.text === 'Noise')?.enabled).toBe(false);
    await userEvent.click(screen.getByRole('button', { name: 'Delete list' }));
    expect(JSON.parse(window.localStorage.getItem(FILTER_LISTS_KEY)!).lists).toEqual([]);
    expect(rules()).toHaveLength(3);
  });
  it('reports native regex errors and prevents applying or saving invalid drafts', async () => {
    vi.mocked(logsApi.validateFilterRules).mockImplementation(async (rules) => {
      if (rules.some((r) => r.mode === 'regex' && r.text === '[')) throw new Error('Invalid filter "[": unclosed character class');
    });
    render(<Editor />);
    await userEvent.selectOptions(screen.getByLabelText('New term match'), 'regex');
    fireEvent.change(screen.getByLabelText('Filter in text'), { target: { value: '[' } });
    expect(await screen.findByRole('alert')).toHaveTextContent('unclosed character class');
    expect(screen.getByRole('button', { name: 'Apply' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Add terms' })).toBeDisabled();
    await add('(?i)Timeout');
    expect(rules()[0]).toMatchObject({ text: '(?i)Timeout', mode: 'regex' });
  });
  it('reports storage failures without claiming that a list was saved', async () => {
    render(<Editor initial={[newRule('error', 'include')]} />);
    const spy = vi.spyOn(window.localStorage, 'setItem').mockImplementation(() => { throw new Error('Storage full'); });
    fireEvent.change(screen.getByLabelText('Filter list name'), { target: { value: 'Errors' } });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save as new' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Save as new' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not save');
    expect(screen.queryByRole('option', { name: 'Errors' })).not.toBeInTheDocument();
    spy.mockRestore();
  });
});
