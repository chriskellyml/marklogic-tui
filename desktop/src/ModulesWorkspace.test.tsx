import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import { ModuleBrowser } from './ModulesWorkspace';
import { modulesApi } from './modules-api';
vi.mock('./modules-api',async(original)=>({...await original<typeof import('./modules-api')>(),modulesApi:{cache:vi.fn(),search:vi.fn(),checkout:vi.fn(),open:vi.fn()}}));
const props={connection:{server:'local',appServer:null,database:'Content'},database:'Modules',selectedPending:false,revision:0,disabled:false,onOpen:vi.fn(),onBusy:vi.fn(),onError:vi.fn()};
beforeEach(()=>{vi.clearAllMocks();vi.mocked(modulesApi.cache).mockResolvedValue([{uri:'/api/get.xqy',dirty:true,unsynced:true}]);vi.mocked(modulesApi.open).mockResolvedValue({content:'2',original:'1'});vi.mocked(modulesApi.search).mockResolvedValue({uris:['/api/new.xqy'],total:1});});
it('marks pending modules and opens them in the shared editor',async()=>{
 render(<ModuleBrowser {...props}/>);
 expect(await screen.findByText('UNSYNCED')).toBeInTheDocument();
 await userEvent.click(screen.getByText('get.xqy'));
 await waitFor(()=>expect(props.onOpen).toHaveBeenCalledWith({connection:props.connection,database:'Modules',uri:'/api/get.xqy'},{content:'2',original:'1'},true));
});
it('downloads selected modules from the compact remote picker',async()=>{
 render(<ModuleBrowser {...props}/>);
 await userEvent.click(screen.getByRole('button',{name:'Find server modules'}));
 expect(await screen.findByText('/api/new.xqy')).toBeInTheDocument();
 await userEvent.click(screen.getByRole('checkbox',{name:'Select all module results'}));
 await userEvent.click(screen.getByRole('button',{name:'Download selected modules'}));
 await waitFor(()=>expect(modulesApi.checkout).toHaveBeenCalledWith({connection:props.connection,database:'Modules',uri:'/api/new.xqy'}));
});

it('searches module URIs independently of the file filter',async()=>{
 render(<ModuleBrowser {...props}/>);
 await userEvent.type(screen.getByRole('textbox',{name:'Search modules'}),'new.xqy');
 await userEvent.click(screen.getByRole('button',{name:'Search'}));
 await waitFor(()=>expect(modulesApi.search).toHaveBeenCalledWith(props.connection,'Modules','new.xqy',0));
});
