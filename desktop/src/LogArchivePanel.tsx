import { useState } from 'react';
import { X } from 'lucide-react';
import { open } from '@tauri-apps/plugin-dialog';
import type { ArchiveSettings } from './logs-api';
import { errorMessage } from './api';

export const defaultArchiveSettings: ArchiveSettings = { folder: '', compressAfterDays: 7, archiveAfterDays: 7 };

export function LogArchivePanel({ initial, busy, message, onSave, onRun, onClose }: {
  initial: ArchiveSettings; busy: boolean; message: string;
  onSave: (settings: ArchiveSettings) => Promise<void>;
  onRun: (settings: ArchiveSettings) => Promise<void>;
  onClose: () => void;
}) {
  const [settings, setSettings] = useState(initial);
  const [error, setError] = useState('');
  const [saving, setSaving] = useState(false);
  const invalid = !settings.folder.trim() || !Number.isInteger(settings.compressAfterDays) || settings.compressAfterDays < 1
    || !Number.isInteger(settings.archiveAfterDays) || settings.archiveAfterDays < settings.compressAfterDays || settings.archiveAfterDays > 36500;
  async function chooseFolder() {
    try {
      const folder = await open({ directory: true, multiple: false, title: 'Choose a mounted archive folder' });
      if (typeof folder === 'string') {
        const next = { ...settings, folder };
        setSettings(next);
        await onSave(next);
      }
    } catch (e) { setError(errorMessage(e)); }
  }
  async function submit(run: boolean) {
    setError(''); setSaving(true);
    try { await (run ? onRun : onSave)({ ...settings, folder: settings.folder.trim() }); }
    catch (e) { setError(errorMessage(e)); }
    finally { setSaving(false); }
  }
  return <section className="log-options-popover log-archive-panel" aria-label="Log archive">
    <div className="options-title"><strong>Log archive</strong><button aria-label="Close log archive" onClick={onClose}><X size={14} /></button></div>
    <p>Keep recent logs local. Compress older days with best gzip compression, then move them to a mounted archive disk.</p>
    <fieldset disabled={busy || saving}>
      <label>Archive folder<div className="log-archive-folder"><input aria-label="Archive folder" placeholder="/Volumes/Archive/MarkLogic" value={settings.folder} onChange={event => setSettings(old => ({ ...old, folder: event.target.value }))} onBlur={() => { if (!invalid && settings.folder.trim() !== initial.folder) void onSave({ ...settings, folder: settings.folder.trim() }).catch(e => setError(errorMessage(e))); }} /><button className="log-tool" onClick={() => void chooseFolder()}>Choose folder…</button></div></label>
      <div className="log-archive-ages">
        <label>Compress days older than<input aria-label="Compress logs older than days" type="number" min="1" max="36500" value={settings.compressAfterDays} onChange={event => setSettings(old => ({ ...old, compressAfterDays: event.target.valueAsNumber }))} /></label>
        <label>Move days older than<input aria-label="Archive logs older than days" type="number" min={settings.compressAfterDays} max="36500" value={settings.archiveAfterDays} onChange={event => setSettings(old => ({ ...old, archiveAfterDays: event.target.valueAsNumber }))} /></label>
      </div>
      <p>The folder is saved for this server and log directory. Default: archive days older than 7 days directly. For two stages, compress after 7 days and move after 14 days. Dates use server time; days without a timestamp stay local.</p>
      <p>Archived logs stay available in Files while the disk is mounted. Archive now also clears the local record index to free space; selecting logs rebuilds their index. Completed files are kept if you cancel.</p>
      <div className="log-archive-actions"><button className="log-tool" disabled={invalid} onClick={() => void submit(false)}>Save settings</button><button className="log-tool" disabled={invalid} onClick={() => void submit(true)}>Archive now</button></div>
    </fieldset>
    {error && <p role="alert">{error}</p>}
    {message && <p role="status">{message}</p>}
  </section>;
}
