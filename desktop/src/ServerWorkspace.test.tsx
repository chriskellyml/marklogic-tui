import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, it, expect, vi } from 'vitest';
import { ServerWorkspace } from './ServerWorkspace';
import { api, type Workspace } from './api';

vi.mock('./api',async (original)=>({...await original<typeof import('./api')>(),api:{serverSettings:vi.fn(),saveServerSettings:vi.fn()}}));
const workspace:Workspace={servers:[{name:'local',endpoint:'http://localhost:8000',appServers:[]}],activeServer:'local',activeAppServer:null,activeDatabase:null,configPath:'/config.toml',queryRoot:'/queries',trackedFolders:[]};

describe('server workspace',()=>{
  it('edits profiles and endpoints without returning or overwriting the stored password',async()=>{
    vi.mocked(api.serverSettings).mockResolvedValue({name:'local',host:'localhost',port:8000,secure:false,username:'admin',authType:'digest',insecure:false,appServers:[]});
    vi.mocked(api.saveServerSettings).mockResolvedValue();
    const saved=vi.fn().mockResolvedValue(undefined);
    render(<ServerWorkspace active workspace={workspace} selectedServer="local" onSaved={saved} onDirtyChange={()=>{}}/>);
    await screen.findByDisplayValue('localhost');
    expect(screen.getByLabelText('Password')).toHaveValue('');
    await userEvent.type(screen.getByLabelText('Environment color'),'#ffcc00');
    await userEvent.clear(screen.getByLabelText('Host'));
    await userEvent.type(screen.getByLabelText('Host'),'example.test');
    await userEvent.click(screen.getByRole('button',{name:'Add app server'}));
    await userEvent.type(screen.getByLabelText('App server name'),'App-Services');
    await userEvent.click(screen.getByRole('button',{name:'Save'}));
    await waitFor(()=>expect(saved).toHaveBeenCalledWith('local'));
    expect(api.saveServerSettings).toHaveBeenCalledWith('local',expect.objectContaining({host:'example.test',color:'#ffcc00',appServers:[expect.objectContaining({name:'App-Services'})]}));
    expect(vi.mocked(api.saveServerSettings).mock.calls[0][1].password).toBeUndefined();
    expect(screen.getByRole('status')).toHaveTextContent('Server settings saved');
  });
});
