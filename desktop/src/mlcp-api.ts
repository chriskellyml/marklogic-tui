import { Channel, invoke } from '@tauri-apps/api/core';
import { confirm, open } from '@tauri-apps/plugin-dialog';
import type { Connection } from './api';

export type Selection = { kind: 'all' } | { kind: 'collections' | 'directories'; value: string };

export type Transfer =
  | { direction: 'pull'; folder: string; selection: Selection; replaceId?: string | null; jobId?: string; replace?: boolean }
  | { direction: 'push'; folder: string; copyPermissions: boolean };

export interface TransferResult {
  archive: string;
  parts: number;
  output: string;
}

export const mlcpApi = {
  previewUris: (connection: Connection, selection: Selection) => invoke<string[]>('mlcp_preview_uris', { connection, selection }),
  jobData: (folder: string, jobId: string) => invoke<{archives: {path: string; size: number; files: number}[]; log: string}>('mlcp_job_data', { folder, jobId }),
  deleteJobData: (folder: string, jobId: string) => invoke<void>('mlcp_delete_job_data', { folder, jobId }),
  archiveSize: (folder: string) => invoke<number>('mlcp_archive_size', { folder }),
  archiveFiles: (folder: string) => invoke<number>('mlcp_archive_files', { folder }),
  deleteDownload: (folder: string, jobId: string, archive: string) => invoke<void>('mlcp_delete_download', { folder, jobId, archive }),
  confirmDeleteDownload: (name: string) => confirm(`Delete the download "${name}" and its files?`, { title: 'Delete download', kind: 'warning', okLabel: 'Delete download', cancelLabel: 'Cancel' }),
  confirmDeleteJob: (name: string) => confirm(`Delete job "${name}"?`, { title: 'Delete job', kind: 'warning', okLabel: 'Delete job', cancelLabel: 'Cancel' }),
  confirmDeleteData: (name: string, folder: string) => confirm(`Also delete the downloaded data and logs for "${name}" in ${folder}?`, { title: 'Delete job data', kind: 'warning', okLabel: 'Delete data', cancelLabel: 'Keep data' }),
  chooseFolder: (title: string) => open({ directory: true, multiple: false, title }),
  confirmPush: (archive: string, destination: string) =>
    confirm(`Import the archive at ${archive} into ${destination}? Documents with matching URIs may be replaced.`, {
      title: 'Push MLCP archive', kind: 'warning', okLabel: 'Push archive', cancelLabel: 'Cancel',
    }),
  transfer: (connection: Connection, transfer: Transfer, onOutput: (output: string) => void) => {
    const events = new Channel<string>();
    events.onmessage = onOutput;
    return invoke<TransferResult>('mlcp_transfer', { connection, transfer, events });
  },
};
