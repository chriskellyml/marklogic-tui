import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { expect, it, vi } from 'vitest';
import { api, type VariableBinding } from './api';
import { ExternalVariables } from './ExternalVariables';
vi.mock('./api', () => ({ api: {queryVariables: vi.fn()}, errorMessage: String }));

it('shows declared types and defaults and inserts a searchable global token', async () => {
  vi.mocked(api.queryVariables).mockResolvedValue({variables: [{name:'limit',sequenceType:'xs:int',defaultValue:'10'}],globals:{LIMIT:'42',OTHER:'abc'}});
  const change = vi.fn();
  function Harness() {
    const [values,setValues] = useState<Record<string,VariableBinding>>({});
    return <ExternalVariables source="declare variable $limit as xs:int external := 10;" values={values} onChange={next=>{setValues(next);change(next);}} disabled={false}/>;
  }
  render(<Harness/>);
  expect(await screen.findByText('xs:int')).toBeInTheDocument();
  expect(screen.getByLabelText('Value for $limit')).toHaveAttribute('placeholder','Default: 10');
  await userEvent.click(screen.getByRole('button',{name:'Choose global for $limit'}));
  await userEvent.type(screen.getByLabelText('Search global values'),'42');
  expect(screen.queryByText('%%OTHER%%')).not.toBeInTheDocument();
  await userEvent.click(screen.getByText('%%LIMIT%%'));
  expect(screen.getByLabelText('Value for $limit')).toHaveValue('%%LIMIT%%');
  await userEvent.click(screen.getByLabelText('XQuery expression'));
  expect(change).toHaveBeenLastCalledWith({limit:{value:'%%LIMIT%%',expression:true}});
  await userEvent.click(screen.getByRole('button',{name:'Reset'}));
  expect(change).toHaveBeenLastCalledWith({});
});

it('hides the panel for queries with no external declarations', async () => {
  vi.mocked(api.queryVariables).mockResolvedValue({variables:[],globals:{}});
  render(<ExternalVariables source="1+1" values={{}} onChange={()=>{}} disabled={false}/>);
  await waitFor(()=>expect(api.queryVariables).toHaveBeenCalledWith('1+1'));
  expect(screen.queryByLabelText('External variables')).not.toBeInTheDocument();
});
