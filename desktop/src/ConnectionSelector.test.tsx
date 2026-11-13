import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, it, vi } from 'vitest';
import { ConnectionSelector } from './ConnectionSelector';
it('stays open while interacting inside and closes on outside click or Escape', async()=>{
 render(<><ConnectionSelector servers={[{name:'local',endpoint:'',appServers:[]}]} connection={{server:'local',appServer:null,database:'Documents'}} disabled={false} onServer={vi.fn()} onAppServer={vi.fn()}><input aria-label="Database" /></ConnectionSelector><button>Outside</button></>);
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
