import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, it, vi } from 'vitest';
import { QueryTree } from './QueryTree';
it('keeps nested folders, empty folders, collapse state and filename filtering', async()=>{
 const onOpen=vi.fn();const items=[{path:'api/user/get.xqy',language:'xquery'},{path:'root.sjs',language:'javascript'}];
 const props={items,folders:['api','api/user','empty'],filter:'',disabled:false,onOpen};
 const view=render(<QueryTree {...props}/>);
 expect(screen.getByRole('button',{name:'Folder empty'})).toBeInTheDocument();
 await userEvent.click(screen.getByRole('button',{name:'Folder api'}));
 expect(screen.queryByText('get.xqy')).not.toBeInTheDocument();
 view.rerender(<QueryTree {...props} filter="get"/>);
 expect(screen.getByRole('button',{name:'Folder api/user'})).toHaveAttribute('aria-expanded','true');
 await userEvent.click(screen.getByText('get.xqy'));
 expect(onOpen).toHaveBeenCalledWith(items[0]);
 expect(screen.queryByText('root.sjs')).not.toBeInTheDocument();
});
