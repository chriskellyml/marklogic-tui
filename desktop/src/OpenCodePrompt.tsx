import { ChevronDown, ChevronRight, LoaderCircle, Play, RefreshCw, Star } from 'lucide-react';
import { memo, useEffect, useId, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { api, errorMessage, type Language, type OpenCodeModel } from './api';

interface Revision {
  original: string;
  language: Language;
  replacement: string;
}

function modelKey(model: OpenCodeModel) {
  return `${model.providerId}/${model.id}`;
}

export const OpenCodePrompt = memo(function OpenCodePrompt({ tabId, active, source, language, disabled, preferredModel, onSelectModel, favoriteModels, onChangeFavorites, onApply }: {
  tabId: string;
  active: boolean;
  source: string;
  language: Language;
  disabled: boolean;
  preferredModel: string;
  onSelectModel: (model: string) => void;
  favoriteModels: string[];
  onChangeFavorites: (models: string[]) => void;
  onApply: (tabId: string, revision: Revision) => void;
}) {
  const panelId = useId();
  const [expanded, setExpanded] = useState(false);
  const [models, setModels] = useState<OpenCodeModel[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [starting, setStarting] = useState(false);
  const [showModels, setShowModels] = useState(false);
  const modelButton = useRef<HTMLButtonElement>(null);
  const [pickerPosition, setPickerPosition] = useState<{ top: number; left: number; width: number } | null>(null);
  const [modelSearch, setModelSearch] = useState('');
  const [prompt, setPrompt] = useState('');
  const [generating, setGenerating] = useState(false);
  const [revision, setRevision] = useState<Revision | null>(null);
  const [history, setHistory] = useState<{ request: string; response: string }[]>([]);
  const [versions, setVersions] = useState<{ label: string; source: string; language: Language }[]>([]);
  const [comparison, setComparison] = useState<number | null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const model = models?.find(item => modelKey(item) === preferredModel);
  const search = modelSearch.trim().toLowerCase();
  const filteredModels = (models ?? []).filter(item => `${item.name} ${item.providerId} ${item.id}`.toLowerCase().includes(search))
    .sort((a, b) => Number(favoriteModels.includes(modelKey(b))) - Number(favoriteModels.includes(modelKey(a))));
  const canGenerate = active && !disabled && !loading && !generating && Boolean(model && prompt.trim());
  const stale = revision && (revision.original !== source || revision.language !== language);

  useEffect(() => {
    if (!showModels || !active) return;
    function positionPicker() {
      const anchor = modelButton.current?.getBoundingClientRect();
      if (!anchor) return;
      const width = Math.min(440, window.innerWidth - 24);
      const left = Math.max(12, Math.min(anchor.right - width, window.innerWidth - width - 12));
      const top = anchor.bottom + 5 + 190 < window.innerHeight ? anchor.bottom + 5 : Math.max(12, anchor.top - 195);
      setPickerPosition({ top, left, width });
    }
    positionPicker();
    window.addEventListener('resize', positionPicker);
    window.addEventListener('scroll', positionPicker, true);
    return () => {
      window.removeEventListener('resize', positionPicker);
      window.removeEventListener('scroll', positionPicker, true);
    };
  }, [showModels, active]);

  async function loadModels() {
    setLoading(true);
    setError('');
    setNotice('');
    try {
      const available = await api.openCodeModels();
      setModels(available);
      if (!available.length) setNotice('Connect a provider in OpenCode and refresh the model list.');
    } catch (error) {
      setModels(null);
      setError(errorMessage(error));
    } finally {
      setLoading(false);
    }
  }

  async function startService() {
    setStarting(true);
    setError('');
    setNotice('Starting the OpenCode service…');
    try {
      await api.openCodeStartService();
      setNotice('OpenCode service started. Loading models…');
      await loadModels();
    } catch (error) {
      setError(errorMessage(error));
    } finally {
      setStarting(false);
    }
  }

  async function generate() {
    if (!canGenerate || !model) return;
    setGenerating(true);
    setError('');
    setNotice('');
    try {
      const request = prompt.trim();
      const replacement = await api.openCodeReviseQuery({ providerId: model.providerId, id: model.id }, request, source, language, history);
      setHistory(old => [...old, { request, response: replacement }]);
      setPrompt('');
      if (replacement === source) setNotice('The model returned the same query.');
      else setRevision({ original: source, language, replacement });
    } catch (error) {
      setError(errorMessage(error));
    } finally {
      setGenerating(false);
    }
  }

  return <section className="opencode-prompt" hidden={!active} aria-label="OpenCode query assistant" onKeyDown={event => {
    if (event.metaKey || event.ctrlKey) event.stopPropagation();
  }}>
    <button className="opencode-toggle" type="button" aria-expanded={expanded} aria-controls={panelId} onClick={() => {
      setExpanded(!expanded);
      if (!expanded && models === null && !loading) void loadModels();
    }}>
      {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />} OpenCode
    </button>
    <div id={panelId} className="opencode-panel" hidden={!expanded}>
      {revision ? <div className="opencode-revision">
        <p id={`${panelId}-revision`}>Proposed query</p>
        <p>Current query</p><pre>{revision.original || '(Empty buffer)'}</pre>
        <pre aria-labelledby={`${panelId}-revision`} tabIndex={0}>{revision.replacement || '(Empty buffer)'}</pre>
        <div className="opencode-actions">
          {stale && <span role="status">The buffer changed. Generate a new revision to use the latest text.</span>}
          <button type="button" onClick={() => setRevision(null)}>Discard revision</button>
          <button className="run-button" type="button" disabled={disabled || Boolean(stale)} onClick={() => {
            setVersions(old => [...old, { label: `Before revision ${old.length + 1}`, source: revision.original, language: revision.language }]);
            onApply(tabId, revision);
            setRevision(null);
            setNotice('Revision applied.');
          }}>Apply to editor</button>
        </div>
      </div> : <form className="opencode-form" onSubmit={event => { event.preventDefault(); void generate(); }}>
        <textarea aria-label="OpenCode prompt" placeholder="What should change in this query?"
          rows={1} value={prompt} disabled={disabled || generating} onChange={event => setPrompt(event.target.value)} />
        <div className="opencode-actions">
          <button ref={modelButton} type="button" className="opencode-model-toggle" aria-expanded={showModels} aria-controls={`${panelId}-models`}
            disabled={loading || generating || !models?.length} onClick={() => setShowModels(!showModels)}>
            <span>{loading ? 'Loading models…' : model ? `${model.name} · ${model.providerId}` : 'Choose a model'}</span><ChevronDown size={14} />
          </button>
          <button type="button" className="icon-button" aria-label="Refresh OpenCode models" title="Refresh OpenCode models"
            disabled={loading || generating} onClick={() => void loadModels()}><RefreshCw size={14} /></button>
          <button type="submit" className="run-button" disabled={!canGenerate}>
            {generating && <LoaderCircle className="spin" size={14} />}{generating ? 'Generating…' : 'Generate revision'}
          </button>
        </div>
      </form>}
      <div className="opencode-history">
        <span>Follow-ups include {history.length} previous {history.length === 1 ? 'exchange' : 'exchanges'} and the current buffer. History lasts while this tab is open.</span>
        {history.length > 0 && <button type="button" disabled={generating} onClick={() => setHistory([])}>New conversation</button>}
        {versions.length > 0 && <label>Compare a previous version <select value={comparison ?? ''} onChange={event => setComparison(event.target.value === '' ? null : Number(event.target.value))}><option value="">Select version</option>{versions.map((version, index) => <option key={index} value={index}>{version.label}</option>)}</select></label>}
        {comparison !== null && versions[comparison] && <div className="opencode-revision">
          <p>Previous version</p><pre>{versions[comparison].source || '(Empty buffer)'}</pre>
          <p>Current buffer</p><pre>{source || '(Empty buffer)'}</pre>
          <button type="button" disabled={disabled || versions[comparison].language !== language} onClick={() => {
            const previous = versions[comparison];
            setVersions(old => [...old, { label: `Before restore ${old.length + 1}`, source, language }]);
            onApply(tabId, { original: source, language, replacement: previous.source });
            setComparison(null);
            setNotice('Previous version restored.');
          }}>Restore previous version</button>
        </div>}
      </div>
      {error && <p className="opencode-error" role="alert">{error}</p>}
      {error && models === null && <p className="opencode-error">In Terminal, run <code>opencode service start</code> and check <code>opencode service status</code>, then refresh the models.</p>}
      {error && models === null && <button type="button" className="opencode-start" disabled={starting || loading}
        onClick={() => void startService()}>{starting ? <LoaderCircle className="spin" size={13} /> : <Play size={13} />}
        {starting ? 'Starting OpenCode…' : 'Start OpenCode service'}</button>}
      <div className="opencode-status" role="status">{generating ? 'Waiting for OpenCode…' : notice}</div>
    </div>
    {active && showModels && pickerPosition && createPortal(<div className="opencode-model-picker opencode-model-picker-portal"
      id={`${panelId}-models`} style={pickerPosition}>
      <input aria-label="Search OpenCode models" placeholder="Search models or providers" value={modelSearch}
        onChange={event => setModelSearch(event.target.value)} />
      <div className="opencode-model-list" aria-label="Available OpenCode models">
        {filteredModels.map(item => {
          const key = modelKey(item);
          const favorite = favoriteModels.includes(key);
          return <div className="opencode-model-row" key={key}>
            <button type="button" className="opencode-model-option" aria-pressed={preferredModel === key}
              onClick={() => { onSelectModel(key); setShowModels(false); }}>
              <span>{item.name}</span><small>{item.providerId} / {item.id}</small>
            </button>
            <button type="button" className="opencode-model-favorite" aria-label={`${favorite ? 'Remove' : 'Add'} ${item.name} ${favorite ? 'from' : 'to'} favorites (${item.providerId})`}
              aria-pressed={favorite} onClick={() => onChangeFavorites(favorite ? favoriteModels.filter(value => value !== key) : [...favoriteModels, key])}>
              <Star size={15} fill={favorite ? 'currentColor' : 'none'} />
            </button>
          </div>;
        })}
        {!filteredModels.length && <p>No matching models.</p>}
      </div>
    </div>, document.body)}
  </section>;
});
