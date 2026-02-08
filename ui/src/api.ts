import type { StatusResponse } from './types';

export async function fetchStatus(): Promise<StatusResponse> {
  const res = await fetch('/api/status');
  if (!res.ok) {
    throw new Error(`status ${res.status}`);
  }
  return (await res.json()) as StatusResponse;
}

async function postJson(path: string, body: unknown): Promise<void> {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(`${path} ${res.status}${text ? `: ${text}` : ''}`);
  }
}

export async function setPaused(paused: boolean): Promise<void> {
  await postJson('/api/control/pause', { paused });
}

export async function setSpeed(proofsPerWindow: number, windowMs: number): Promise<void> {
  await postJson('/api/control/speed', { proofs_per_window: proofsPerWindow, window_ms: windowMs });
}

export async function setCursor(cursorEventId: number, endEventId: number | null): Promise<void> {
  await postJson('/api/control/cursor', { cursor_event_id: cursorEventId, end_event_id: endEventId });
}
