import { api, type Connection, type Language, type QueryFile, type QueryResult, type VariableBinding } from './api';
import { modulesApi, type ModuleContext } from './modules-api';
import { defaultSessionId, readStored, sessionKey } from './workbench-sessions';
export interface EditorTab {
  variableValues?: Record<string, VariableBinding>;
  id:string; file:QueryFile|null; language:Language; source:string; savedSource:string;
  moduleContext:ModuleContext|null; moduleOriginal:string; modulePending:boolean; compare:boolean;
  result:QueryResult|null; resultContext:string; resultConnection:Connection|null; page:number;
}
export const queryVariableKey = (id: string) => `workbench.query-variables.v1:${id}`;

export function createTab(id:string, source:string, file:QueryFile|null=null, language:Language='javascript', sessionId=defaultSessionId):EditorTab {
  let variableValues: Record<string, VariableBinding> = {};
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(sessionKey(sessionId, queryVariableKey(id))) ?? '{}');
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

interface SavedEditorTab {
  id: string;
  file: QueryFile | null;
  language: Language;
  moduleContext: ModuleContext | null;
  compare: boolean;
  variableValues?: Record<string, VariableBinding>;
  source?: string;
  savedSource?: string;
}

interface SavedEditorState {
  activeId: string;
  tabs: SavedEditorTab[];
}

export const editorStateKey = 'workbench.editor-tabs.v1';

export function saveEditorState(key: string, tabs: EditorTab[], activeId: string) {
  const saved: SavedEditorState = {
    activeId,
    tabs: tabs.map(({ id, file, language, moduleContext, compare, variableValues, source, savedSource }) => ({
      id, file, language, moduleContext, compare, variableValues,
      ...(!file || source !== savedSource ? { source, savedSource } : {}),
    })),
  };
  const serialized = JSON.stringify(saved);
  if (localStorage.getItem(key) !== serialized) localStorage.setItem(key, serialized);
}

export async function restoreEditorState(sessionId: string, starter: string, queryRootAvailable: boolean) {
  const saved = readStored<SavedEditorState | null>(sessionKey(sessionId, editorStateKey), null);
  if (!saved || !Array.isArray(saved.tabs)) {
    const tabs = sessionId === defaultSessionId ? restoreScratchTabs(starter) :
      [createTab('scratch-1', starter, null, 'javascript', sessionId)];
    return { tabs, activeId: tabs[0].id, missing: [] };
  }

  const missing: string[] = [];
  const restored = await Promise.all(saved.tabs.filter(tab => tab && typeof tab.id === 'string' &&
    ['javascript', 'xquery', 'text'].includes(tab.language)).map(async tab => {
    const build = (source: string, file: QueryFile | null) => ({
      ...createTab(tab.id, source, file, tab.language, sessionId),
      variableValues: tab.variableValues ?? {},
    });
    if (!tab.file) return build(tab.source ?? '', null);
    try {
      const module = tab.moduleContext ? await modulesApi.open(tab.moduleContext) : null;
      if (!module && !queryRootAvailable) throw new Error('Query folder unavailable');
      const latest = module?.content ?? await api.openQuery(tab.file.path);
      const hasDraft = typeof tab.source === 'string' && typeof tab.savedSource === 'string' && tab.source !== latest;
      return {
        ...build(hasDraft ? tab.source! : latest, tab.file),
        savedSource: hasDraft ? tab.savedSource! : latest,
        moduleContext: tab.moduleContext,
        moduleOriginal: module?.original ?? '',
        modulePending: Boolean(module && module.content !== module.original),
        compare: tab.compare,
      };
    } catch {
      missing.push(tab.file.path);
      // Keep unsaved text recoverable even when its original file has moved.
      return typeof tab.source === 'string' ? build(tab.source, null) : null;
    }
  }));
  const tabs: EditorTab[] = restored.filter(tab => tab !== null);
  if (!tabs.length) tabs.push(createTab('scratch-1', starter, null, 'javascript', sessionId));
  return { tabs, activeId: tabs.some(tab => tab.id === saved.activeId) ? saved.activeId : tabs[0].id, missing };
}
