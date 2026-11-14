import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, it, vi } from 'vitest';
import { ConnectionSelector } from './ConnectionSelector';
it('stays open while interacting inside and closes on outside click or Escape', async()=>{
 render(<><ConnectionSelector servers={[{name:'local',endpoint:'',appServers:[]}]} connection={{server:'local',appServer:null,database:'Documents',modulesDatabase:'Modules'}} disabled={false} onApply={vi.fn()}>{()=><input aria-label="Database" />}</ConnectionSelector><button>Outside</button></>);
 const summary=screen.getByTitle('Choose connection');
 await userEvent.click(summary);
 expect(screen.getByRole('region',{name:'Connection settings'})).toBeVisible();
 await userEvent.click(screen.getByLabelText('Database'));
 expect(screen.getByRole('region',{name:'Connection settings'})).toBeVisible();
 await userEvent.click(screen.getByRole('button',{name:'Outside'}));
 expect(summary.closest('details')).not.toHaveAttribute('open');
 await userEvent.click(summary);
 await userEvent.keyboard('{Escape}');
 expect(summary.closest('details')).not.toHaveAttribute('open');
 expect(summary).toHaveFocus();
});

it('applies a complete server and app selection once, and discards cancelled choices', async () => {
 const onApply=vi.fn();
 const connection={server:'local',appServer:null,database:'Documents',modulesDatabase:'Modules'};
 render(<ConnectionSelector servers={[{name:'local',endpoint:'',appServers:[]},{name:'stage',endpoint:'',appServers:[{name:'api',port:8000,secure:false,content_database:'Content',modules_database:'Code'}]}]} connection={connection} disabled={false} onApply={onApply}/>);
 await userEvent.click(screen.getByTitle('Choose connection'));
 await userEvent.selectOptions(screen.getByLabelText('Server'),'stage');
 await userEvent.selectOptions(screen.getByLabelText('App server'),'api');
 expect(onApply).not.toHaveBeenCalled();
 expect(screen.getByTitle('Choose connection')).toHaveTextContent('local');
 await userEvent.click(screen.getByRole('button',{name:'Cancel'}));
 await userEvent.click(screen.getByTitle('Choose connection'));
 expect(screen.getByLabelText('Server')).toHaveValue('local');
 await userEvent.selectOptions(screen.getByLabelText('Server'),'stage');
 await userEvent.selectOptions(screen.getByLabelText('App server'),'api');
 await userEvent.click(screen.getByRole('button',{name:'OK'}));
 expect(onApply).toHaveBeenCalledExactlyOnceWith({server:'stage',appServer:'api',database:'Content',modulesDatabase:'Code'});
});
