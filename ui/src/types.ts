export type TimerSnapshot = {
  count: number;
  total_ms: number;
  max_ms: number;
  avg_ms: number;
};

export type PeerStatus = {
  addr: string;
  connected: boolean;
  reported_peers_known: boolean;
  reported_peer_count: number;
};

export type SendMode = 'announcement' | 'respond_only';

export type InjectorSnapshot = {
  started_ts: number;
  updated_ts: number;

  events_per_minute: number;
  proofs_per_window: number;
  window_ms: number;
  window_tokens_remaining: number;
  targets_total: number;
  connected_peers: number;
  paused: boolean;
  end_event_id: number | null;
  send_mode: SendMode;

  cursor_event_id: number;
  first_event_id: number;
  last_event_id: number;
  peer_statuses: PeerStatus[];
  backlog_events: number;
  inflight_event_id: number | null;

  last_error: string | null;
};

export type ProofsSnapshot = {
  requests_total: number;
  respond_send_ok_total: number;
  respond_send_err_total: number;
  fetch_ok_total: number;
  fetch_not_found_total: number;
  fetch_err_total: number;
  announcement_request_match_total: number;
  announcement_request_miss_total: number;
  announcement_to_request_ms: TimerSnapshot;
};

export type StatusResponse = {
  injector: InjectorSnapshot;
  proofs: ProofsSnapshot;
};
