import type { Connection, Language, QueryFile, QueryResult, VariableBinding } from './api';
import type { ModuleContext } from './modules-api';
export interface EditorTab {
  variableValues?: Record<string, VariableBinding>;
  id:string; file:QueryFile|null; language:Language; source:string; savedSource:string;
  moduleContext:ModuleContext|null; moduleOriginal:string; modulePending:boolean; compare:boolean;
  result:QueryResult|null; resultContext:string; resultConnection:Connection|null; page:number;
}
export function createTab(id:string, source:string, file:QueryFile|null=null, language:Language='javascript'):EditorTab {
  let variableValues: Record<string, VariableBinding> = {};
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(`workbench.query-variables.v1:${id}`) ?? '{}');
    if (stored && typeof stored === 'object' && !Array.isArray(stored)) variableValues = Object.fromEntries(Object.entries(stored).filter((entry): entry is [string, VariableBinding] => Boolean(entry[1] && typeof entry[1].value === 'string' && (entry[1].expression === undefined || typeof entry[1].expression === 'boolean'))));
  } catch { /* Session values still work. */ }
  return {variableValues,id,file,language,source,savedSource:source,moduleContext:null,moduleOriginal:'',modulePending:false,compare:false,result:null,resultContext:'',resultConnection:null,page:0};
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

export function saveVariableValues(id: string, values: Record<string, VariableBinding>) {
  try { localStorage.setItem(`workbench.query-variables.v1:${id}`, JSON.stringify(values)); } catch { /* Keep values for this session. */ }
}
