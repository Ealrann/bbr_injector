<script lang="ts">
  import { onMount } from 'svelte';
  import type { SendMode, StatusResponse } from './types';
  import { fetchStatus, setCursor, setMode, setPaused, setSpeed } from './api';

  let loading = $state(false);
  let controlBusy = $state(false);
  let error: string | null = $state(null);
  let status: StatusResponse | null = $state(null);

  let cursorEventId = $state<number | ''>('');
  let endEventId = $state<number | ''>('');
  let speedProofs = $state<number>(1);
  let speedWindowSec = $state<number>(1);
  let sendMode = $state<SendMode>('announcement');
  let rollingAvgMs60 = $state<number | null>(null);
  let rollingSamples60 = $state<number>(0);
  let rollingMissRatio60 = $state<number | null>(null);

  const ROLLING_WINDOW_MS = 60_000;
  const rollingHistory: Array<{
    atMs: number;
    totalMs: number;
    count: number;
    match: number;
    miss: number;
  }> = [];

  async function refresh() {
    loading = true;
    error = null;
    try {
      status = await fetchStatus();
      if (status) {
        // Keep the control form roughly in sync with current state (manual-refresh UI).
        cursorEventId = status.injector.cursor_event_id ?? '';
        endEventId = status.injector.end_event_id ?? '';
        speedProofs = status.injector.proofs_per_window || 1;
        speedWindowSec = Math.max(0.1, (status.injector.window_ms || 1000) / 1000);
        sendMode = status.injector.send_mode || 'announcement';
        updateRollingStats(status);
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    void refresh();
  });

  function fmtTs(ts: number): string {
    if (!ts) return '-';
    return new Date(ts * 1000).toLocaleString();
  }

  function remainingEvents(): string {
    if (!status) return '—';
    const rangeEndVal = status.injector.end_event_id ?? 0;
    const last = rangeEndVal || status.injector.last_event_id || 0;
    const cur = status.injector.cursor_event_id ?? 0;
    return last > cur ? String(last - cur) : '0';
  }

  function updateRollingStats(nextStatus: StatusResponse): void {
    const now = Date.now();
    const timer = nextStatus.proofs.announcement_to_request_ms;
    const match = nextStatus.proofs.announcement_request_match_total;
    const miss = nextStatus.proofs.announcement_request_miss_total;

    const latest = {
      atMs: now,
      totalMs: timer.total_ms,
      count: timer.count,
      match,
      miss
    };

    const prev = rollingHistory[rollingHistory.length - 1];
    if (
      prev &&
      (latest.totalMs < prev.totalMs ||
        latest.count < prev.count ||
        latest.match < prev.match ||
        latest.miss < prev.miss)
    ) {
      rollingHistory.length = 0;
    }

    rollingHistory.push(latest);

    while (rollingHistory.length > 0 && now - rollingHistory[0].atMs > ROLLING_WINDOW_MS) {
      rollingHistory.shift();
    }

    const baseline = rollingHistory[0];
    const countDelta = latest.count - baseline.count;
    const totalMsDelta = latest.totalMs - baseline.totalMs;
    rollingSamples60 = countDelta;
    rollingAvgMs60 = countDelta > 0 ? totalMsDelta / countDelta : null;

    const matchDelta = latest.match - baseline.match;
    const missDelta = latest.miss - baseline.miss;
    const totalReqDelta = matchDelta + missDelta;
    rollingMissRatio60 = totalReqDelta > 0 ? missDelta / totalReqDelta : null;
  }

  async function setPausedUi(paused: boolean) {
    controlBusy = true;
    error = null;
    try {
      await setPaused(paused);
      await refresh();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      controlBusy = false;
    }
  }

  async function applyCursor() {
    if (cursorEventId === '') {
      error = 'cursor must be set';
      return;
    }
    controlBusy = true;
    error = null;
    try {
      await setCursor(cursorEventId, endEventId === '' ? null : endEventId);
      await refresh();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      controlBusy = false;
    }
  }

  async function applySpeed() {
    const proofs = Math.max(1, Math.floor(speedProofs));
    const normalizedWindowSec = Number.isFinite(speedWindowSec) ? speedWindowSec : 1;
    const windowMs = Math.max(100, Math.round(normalizedWindowSec * 1000));
    controlBusy = true;
    error = null;
    try {
      await setSpeed(proofs, windowMs);
      await refresh();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      controlBusy = false;
    }
  }

  async function applyMode() {
    controlBusy = true;
    error = null;
    try {
      await setMode(sendMode);
      await refresh();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      controlBusy = false;
    }
  }
</script>

<div class="min-h-screen bg-zinc-950 text-zinc-100">
  <header class="border-b border-zinc-800">
    <div class="mx-auto max-w-3xl px-4 py-6">
      <div class="flex items-center justify-between gap-4">
        <div>
          <div class="text-xl font-semibold">BBR Injector</div>
          <div class="text-sm text-zinc-400">Fixed-peer compact-proof injection</div>
        </div>
        <button
          class="rounded bg-zinc-100 px-3 py-2 text-sm font-medium text-zinc-900 disabled:opacity-50"
          onclick={refresh}
          disabled={loading}
        >
          {loading ? 'Refreshing…' : 'Refresh'}
        </button>
      </div>
    </div>
  </header>

  <main class="mx-auto max-w-3xl px-4 py-6 space-y-6">
    {#if error}
      <div class="rounded border border-red-900 bg-red-950/30 px-4 py-3 text-sm text-red-200">
        Error: {error}
      </div>
    {/if}

    {#if status?.injector?.last_error}
      <div class="rounded border border-amber-900 bg-amber-950/30 px-4 py-3 text-sm text-amber-200">
        <div class="font-medium">Injector error</div>
        <div class="mt-1 whitespace-pre-wrap break-words text-xs text-amber-200/90">
          {status.injector.last_error}
        </div>
      </div>
    {/if}

    <section class="rounded border border-zinc-800 bg-zinc-900/30">
      <div class="border-b border-zinc-800 px-4 py-3">
        <div class="text-sm font-semibold">Control</div>
      </div>
      <div class="space-y-4 p-4">
        <div class="flex flex-wrap items-end justify-between gap-3">
          <div>
            <div class="text-xs text-zinc-400">State</div>
            <div class="text-lg font-semibold">
              {status ? (status.injector.paused ? 'Paused' : 'Running') : '—'}
            </div>
            <div class="text-xs text-zinc-500">tokens {status ? status.injector.window_tokens_remaining : '—'}</div>
          </div>
          <div class="flex items-center gap-2">
            <button
              class="rounded bg-zinc-100 px-3 py-2 text-sm font-medium text-zinc-900 disabled:opacity-50"
              onclick={() => setPausedUi(!(status?.injector?.paused ?? true))}
              disabled={controlBusy || loading || !status}
            >
              {status ? (status.injector.paused ? 'Start' : 'Pause') : '…'}
            </button>
          </div>
        </div>

        <div class="grid grid-cols-1 gap-3 md:grid-cols-3">
          <div>
            <div class="text-xs text-zinc-400">Cursor (event_id)</div>
            <input
              class="mt-1 w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100"
              type="number"
              bind:value={cursorEventId}
              min="0"
              step="1"
            />
          </div>
          <div>
            <div class="text-xs text-zinc-400">End (event_id)</div>
            <input
              class="mt-1 w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100"
              type="number"
              bind:value={endEventId}
              min="0"
              step="1"
            />
          </div>
          <div class="flex items-end">
            <button
              class="w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm font-medium text-zinc-100 disabled:opacity-50"
              onclick={applyCursor}
              disabled={controlBusy || loading}
            >
              Apply cursor/end
            </button>
          </div>
        </div>

        <div class="grid grid-cols-1 gap-3 md:grid-cols-3">
          <div>
            <div class="text-xs text-zinc-400">Gossip mode</div>
            <select
              class="mt-1 w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100"
              bind:value={sendMode}
            >
              <option value="announcement">Original mode (announcement)</option>
              <option value="respond_only">Injection mode (RespondCompactVDF only)</option>
            </select>
          </div>
          <div class="md:col-span-2 flex items-end">
            <button
              class="w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm font-medium text-zinc-100 disabled:opacity-50"
              onclick={applyMode}
              disabled={controlBusy || loading}
            >
              Apply mode
            </button>
          </div>
        </div>

        <div class="grid grid-cols-1 gap-3 md:grid-cols-3">
          <div>
            <div class="text-xs text-zinc-400">Proofs per window</div>
            <input
              class="mt-1 w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100"
              type="number"
              bind:value={speedProofs}
              min="1"
              step="1"
            />
          </div>
          <div>
            <div class="text-xs text-zinc-400">Window (seconds)</div>
            <input
              class="mt-1 w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100"
              type="number"
              bind:value={speedWindowSec}
              min="0.1"
              step="0.1"
            />
          </div>
          <div class="flex items-end">
            <button
              class="w-full rounded border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm font-medium text-zinc-100 disabled:opacity-50"
              onclick={applySpeed}
              disabled={controlBusy || loading}
            >
              Apply speed
            </button>
          </div>
        </div>

        <div class="text-xs text-zinc-500">
          Current mode: {status ? status.injector.send_mode : '—'}
          · speed {status ? `${status.injector.proofs_per_window} proof(s) / ${(status.injector.window_ms / 1000).toFixed(2)}s` : '—'}
          · approx {status ? `${status.injector.events_per_minute}` : '—'} / min
        </div>
      </div>
    </section>

    <section class="rounded border border-zinc-800 bg-zinc-900/30">
      <div class="border-b border-zinc-800 px-4 py-3">
        <div class="text-sm font-semibold">Progress</div>
      </div>
      <div class="grid grid-cols-2 gap-4 p-4 md:grid-cols-3">
        <div>
          <div class="text-xs text-zinc-400">Cursor</div>
          <div class="text-2xl font-semibold">{status ? `${status.injector.cursor_event_id}` : '—'}</div>
          <div class="text-xs text-zinc-500">
            remaining {status ? remainingEvents() : '—'} · first {status ? status.injector.first_event_id : '—'} / last
            {status ? status.injector.last_event_id : '—'}
          </div>
        </div>
        <div>
          <div class="text-xs text-zinc-400">Peers</div>
          <div class="text-2xl font-semibold">
            {status ? `${status.injector.connected_peers}` : '—'}
          </div>
          <div class="text-xs text-zinc-500">targets {status ? status.injector.targets_total : '—'}</div>
        </div>
        <div>
          <div class="text-xs text-zinc-400">Rate</div>
          <div class="text-2xl font-semibold">
            {status ? `${status.injector.events_per_minute}` : '—'}
          </div>
          <div class="text-xs text-zinc-500">approx events/min</div>
        </div>
      </div>
      <div class="border-t border-zinc-800 px-4 py-3 text-xs text-zinc-500">
        backlog {status ? status.injector.backlog_events : '—'} · inflight
        {status ? (status.injector.inflight_event_id ?? '—') : '—'} · updated
        {status ? fmtTs(status.injector.updated_ts) : '—'}
      </div>
      {#if status?.injector?.peer_statuses?.length}
        <div class="border-t border-zinc-800 px-4 py-3">
          <div class="mb-2 text-xs text-zinc-400">Peer connectivity</div>
          <div class="space-y-1">
            {#each status.injector.peer_statuses as peer}
              <div class="rounded border border-zinc-800 px-2 py-1">
                <div class="flex items-center justify-between gap-2 text-xs">
                  <div class="font-mono text-zinc-300">{peer.addr}</div>
                  <div class={peer.connected ? 'text-emerald-300' : 'text-zinc-500'}>
                    {peer.connected ? 'Connected' : 'Disconnected'}
                  </div>
                </div>
                <div class="mt-1 text-[11px] text-zinc-500">
                  reported peers {peer.reported_peers_known ? peer.reported_peer_count : '—'}
                </div>
              </div>
            {/each}
          </div>
        </div>
      {/if}
    </section>

    <section class="rounded border border-zinc-800 bg-zinc-900/30">
      <div class="border-b border-zinc-800 px-4 py-3">
        <div class="text-sm font-semibold">Proof serving</div>
      </div>
      <div class="grid grid-cols-2 gap-4 p-4 md:grid-cols-3">
        <div>
          <div class="text-xs text-zinc-400">Requests</div>
          <div class="text-2xl font-semibold">{status ? `${status.proofs.requests_total}` : '—'}</div>
          <div class="text-xs text-zinc-500">request_compact_vdf</div>
        </div>
        <div>
          <div class="text-xs text-zinc-400">Respond OK</div>
          <div class="text-2xl font-semibold">{status ? `${status.proofs.respond_send_ok_total}` : '—'}</div>
          <div class="text-xs text-zinc-500">respond_compact_vdf</div>
        </div>
        <div>
          <div class="text-xs text-zinc-400">Respond ERR</div>
          <div class="text-2xl font-semibold">{status ? `${status.proofs.respond_send_err_total}` : '—'}</div>
          <div class="text-xs text-zinc-500">send failures</div>
        </div>
      </div>
      <div class="border-t border-zinc-800 px-4 py-3 text-xs text-zinc-500">
        fetch ok {status ? status.proofs.fetch_ok_total : '—'} · not_found
        {status ? status.proofs.fetch_not_found_total : '—'} · err {status ? status.proofs.fetch_err_total : '—'}
        <br />
        reaction match {status ? status.proofs.announcement_request_match_total : '—'} · miss
        {status ? status.proofs.announcement_request_miss_total : '—'} · avg
        {status ? `${status.proofs.announcement_to_request_ms.avg_ms.toFixed(1)}ms` : '—'} · max
        {status ? `${status.proofs.announcement_to_request_ms.max_ms}ms` : '—'}
        <br />
        rolling 60s avg {rollingAvgMs60 !== null ? `${rollingAvgMs60.toFixed(1)}ms` : '—'} · samples
        {rollingSamples60} · miss ratio
        {rollingMissRatio60 !== null ? `${(rollingMissRatio60 * 100).toFixed(1)}%` : '—'}
      </div>
    </section>

    <div class="text-xs text-zinc-600">
      started {status ? fmtTs(status.injector.started_ts) : '—'}
    </div>
  </main>
</div>
