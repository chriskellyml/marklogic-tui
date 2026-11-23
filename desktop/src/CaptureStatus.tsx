import type { Capture, CaptureError } from './host-capture';

export function CaptureStatus({ capture, fromCache, busy, error }: {
  capture: Capture<unknown> | null;
  fromCache: boolean;
  busy: boolean;
  error: CaptureError | null;
}) {
  const time = capture ? new Date(capture.capturedAt).toLocaleString() : '';
  if (error) {
    const state = error.offline ? 'Offline' : 'Could not refresh from MarkLogic';
    return <div className="capture-status" role={capture && error.offline ? 'status' : 'alert'}>
      <strong>{state}.</strong> {capture ? `Showing data captured ${time}.` : 'No local capture is available.'}
      <details><summary>Connection details</summary>{error.message}</details>
    </div>;
  }
  if (capture?.cacheWarning) return <p className="capture-status" role="status">The live data could not be saved for offline use. {capture.cacheWarning}</p>;
  if (fromCache && capture) return <p className="capture-status" role="status">Showing data captured {time}.{busy ? ' Checking MarkLogic…' : ''}</p>;
  return null;
}
