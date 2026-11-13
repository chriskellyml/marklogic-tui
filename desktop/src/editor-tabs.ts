import type { Connection, Language, QueryFile, QueryResult } from './api';
import type { ModuleContext } from './modules-api';
export interface EditorTab {
  id:string; file:QueryFile|null; language:Language; source:string; savedSource:string;
  moduleContext:ModuleContext|null; moduleOriginal:string; modulePending:boolean; compare:boolean;
  result:QueryResult|null; resultContext:string; resultConnection:Connection|null; page:number;
}
export function createTab(id:string, source:string, file:QueryFile|null=null, language:Language='javascript'):EditorTab {
  return {id,file,language,source,savedSource:source,moduleContext:null,moduleOriginal:'',modulePending:false,compare:false,result:null,resultContext:'',resultConnection:null,page:0};
}
export function moduleTabId(context:ModuleContext) {return `module:${JSON.stringify([context.connection.server,context.connection.appServer,context.database,context.uri])}`;}

const SCRATCH_KEY='workbench.scratch-tabs.v1';
interface ScratchDraft { id:string; source:string; language:Language }
function readDrafts():ScratchDraft[] {
 const value=window.localStorage.getItem(SCRATCH_KEY);if(!value)return [];
 const parsed:unknown=JSON.parse(value);
 if(!Array.isArray(parsed))return [];
 return parsed.filter((draft):draft is ScratchDraft=>Boolean(draft&&typeof draft.id==='string'&&typeof draft.source==='string'&&['javascript','xquery','text'].includes(draft.language)));
}
export function restoreScratchTabs(fallback:string):EditorTab[] {
 try {const drafts=readDrafts();if(drafts.length)return drafts.map((draft)=>createTab(draft.id,draft.source,null,draft.language));}catch{/* Start fresh if storage is unavailable. */}
 return [createTab('scratch-1',fallback)];
}
export function saveScratchTab(tab:EditorTab) {
 const drafts=readDrafts().filter((draft)=>draft.id!==tab.id);
 drafts.push({id:tab.id,source:tab.source,language:tab.language});
 window.localStorage.setItem(SCRATCH_KEY,JSON.stringify(drafts));
}
export function removeScratchTab(id:string) {window.localStorage.setItem(SCRATCH_KEY,JSON.stringify(readDrafts().filter((draft)=>draft.id!==id)));}
