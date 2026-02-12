use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::archive_db::{ArchiveDb, CompactionEvent};
use crate::chia_messages::{
    build_new_compact_vdf_message, build_respond_compact_vdf_message, resolve_socket_addrs,
};
use crate::config::Config;
use crate::in_flight::{InFlightStore, SendMode};
use crate::peer::{OutboundMessage, PeerEvent, PeerSession};
use crate::proofs::{ProofProvider, ProofServingMetrics};
use crate::shutdown::Shutdown;

const POLL_INTERVAL_MS: u64 = 500;
const EVENTS_READ_LIMIT: u32 = 5_000;

#[derive(Debug, Clone)]
pub enum ControlCommand {
    SetPaused {
        paused: bool,
    },
    SetSpeed {
        proofs_per_window: u32,
        window_ms: u64,
    },
    SetCursorAndEnd {
        cursor_event_id: u64,
        end_event_id: Option<u64>,
    },
    SetSendMode {
        send_mode: SendMode,
    },
}

#[derive(Debug)]
struct Inflight {
    event_id: u64,
    created_at: tokio::time::Instant,
    msg_bytes: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PeerStatusSnapshot {
    pub addr: String,
    pub connected: bool,
    pub reported_peers_known: bool,
    pub reported_peer_count: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InjectorSnapshot {
    pub started_ts: u64,
    pub updated_ts: u64,

    pub events_per_minute: u64,
    pub proofs_per_window: u32,
    pub window_ms: u64,
    pub window_tokens_remaining: u32,
    pub targets_total: usize,
    pub connected_peers: usize,
    pub paused: bool,
    pub end_event_id: Option<u64>,
    pub send_mode: SendMode,

    pub cursor_event_id: u64,
    pub sent_events_total: u64,
    pub first_event_id: u64,
    pub last_event_id: u64,
    pub peer_statuses: Vec<PeerStatusSnapshot>,
    pub backlog_events: usize,
    pub inflight_event_id: Option<u64>,

    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct InjectorHandle {
    pub snapshot_rx: watch::Receiver<InjectorSnapshot>,
    pub control_tx: mpsc::Sender<ControlCommand>,
}

pub struct Injector {
    archive: Arc<ArchiveDb>,
    in_flight: InFlightStore,

    paused: bool,
    end_event_id: Option<u64>,
    send_mode: SendMode,

    proofs_per_window: u32,
    window: Duration,
    window_tokens_remaining: u32,
    window_started_at: tokio::time::Instant,

    cursor_event_id: u64,
    sent_events_total: u64,

    event_buf: VecDeque<CompactionEvent>,
    first_event_id: u64,
    last_event_id: u64,
    last_error: Option<String>,

    peers: HashMap<SocketAddr, mpsc::Sender<OutboundMessage>>,
    connected_peers: HashSet<SocketAddr>,
    remote_peer_counts: HashMap<SocketAddr, usize>,

    inflight: Option<Inflight>,

    started_ts: u64,
    snapshot_tx: watch::Sender<InjectorSnapshot>,
    peer_events: mpsc::Receiver<PeerEvent>,
    control_rx: mpsc::Receiver<ControlCommand>,
}

impl Injector {
    pub async fn start(
        config: Config,
        in_flight: InFlightStore,
        archive: Arc<ArchiveDb>,
        proof_provider: Arc<dyn ProofProvider>,
        proof_metrics: Arc<ProofServingMetrics>,
    ) -> anyhow::Result<(Self, InjectorHandle)> {
        let in_flight_snapshot = in_flight.snapshot().await;
        let cursor_event_id = in_flight_snapshot.cursor_event_id;
        let end_event_id = in_flight_snapshot.end_event_id;
        let started_ts = unix_ts();

        let (control_tx, control_rx) = mpsc::channel::<ControlCommand>(128);

        let proofs_per_window = in_flight_snapshot.proofs_per_window.max(1);
        let window_ms = in_flight_snapshot.window_ms.max(1);
        let window = Duration::from_millis(window_ms);
        let window_tokens_remaining = proofs_per_window;
        let window_started_at = tokio::time::Instant::now();

        let events_per_minute = (proofs_per_window as u64)
            .saturating_mul(60_000)
            .saturating_div(window_ms);
        let (snapshot_tx, snapshot_rx) = watch::channel(InjectorSnapshot {
            started_ts,
            updated_ts: started_ts,
            events_per_minute,
            proofs_per_window,
            window_ms,
            window_tokens_remaining,
            targets_total: 0,
            connected_peers: 0,
            paused: in_flight_snapshot.paused,
            end_event_id,
            send_mode: in_flight_snapshot.send_mode,
            cursor_event_id,
            sent_events_total: 0,
            first_event_id: 0,
            last_event_id: 0,
            peer_statuses: Vec::new(),
            backlog_events: 0,
            inflight_event_id: None,
            last_error: None,
        });

        let (event_tx, peer_events) = mpsc::channel(16_384);

        let mut peers = HashMap::new();
        let mut resolved = HashSet::<SocketAddr>::new();
        for s in &config.peers.targets {
            let addrs = resolve_socket_addrs(s)
                .await
                .with_context(|| format!("resolve peer target {s}"))?;
            if addrs.is_empty() {
                warn!("peer target resolved to 0 addresses: {s}");
            }
            resolved.extend(addrs);
        }
        if resolved.is_empty() {
            return Err(anyhow::anyhow!("no peer targets could be resolved"));
        }
        for addr in resolved {
            let (tx, rx) = mpsc::channel::<OutboundMessage>(1024);
            peers.insert(addr, tx);
            spawn_peer_task(
                addr,
                config.clone(),
                rx,
                event_tx.clone(),
                proof_provider.clone(),
                proof_metrics.clone(),
            );
        }
        snapshot_tx.send_modify(|s| s.targets_total = peers.len());

        Ok((
            Self {
                archive,
                in_flight,
                paused: in_flight_snapshot.paused,
                end_event_id,
                send_mode: in_flight_snapshot.send_mode,
                proofs_per_window,
                window,
                window_tokens_remaining,
                window_started_at,
                cursor_event_id,
                sent_events_total: 0,
                event_buf: VecDeque::new(),
                first_event_id: 0,
                last_event_id: 0,
                last_error: None,
                peers,
                connected_peers: HashSet::new(),
                remote_peer_counts: HashMap::new(),
                inflight: None,
                started_ts,
                snapshot_tx,
                peer_events,
                control_rx,
            },
            InjectorHandle {
                snapshot_rx,
                control_tx,
            },
        ))
    }

    pub async fn run(mut self, shutdown: Shutdown) -> anyhow::Result<()> {
        let mut poll_tick = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
        // Scheduler tick doesn't define throughput; it just drives progress when we're not
        // receiving peer events.
        let mut sched_tick = tokio::time::interval(Duration::from_millis(100));

        info!(
            "Injector starting (cursor_event_id={}, targets={})",
            self.cursor_event_id,
            self.peers.len()
        );
        self.publish_snapshot();

        loop {
            tokio::select! {
                _ = poll_tick.tick() => {
                    self.maybe_refill_events().await;
                    self.maybe_retry_inflight().await;
                    self.publish_snapshot();
                }
                _ = sched_tick.tick() => {
                    self.maybe_send_next().await;
                    self.publish_snapshot();
                }
                Some(evt) = self.peer_events.recv() => {
                    self.on_peer_event(evt).await;
                    self.maybe_send_next().await;
                    self.publish_snapshot();
                }
                Some(cmd) = self.control_rx.recv() => {
                    self.on_control_command(cmd).await;
                    self.maybe_send_next().await;
                    self.publish_snapshot();
                }
                _ = shutdown.wait() => break,
            }
        }

        // Best-effort flush cursor.
        let _ = self
            .in_flight
            .set_cursor_event_id(self.cursor_event_id)
            .await;
        Ok(())
    }

    async fn on_peer_event(&mut self, evt: PeerEvent) {
        match evt {
            PeerEvent::Connected { addr } => {
                self.connected_peers.insert(addr);
            }
            PeerEvent::Disconnected { addr } => {
                self.connected_peers.remove(&addr);
                self.remote_peer_counts.remove(&addr);
            }
            PeerEvent::PeerListReceived { addr, peer_count } => {
                self.remote_peer_counts.insert(addr, peer_count);
            }
            PeerEvent::AnnouncementSent { event_id } => {
                let Some(inflight) = &self.inflight else {
                    return;
                };
                if inflight.event_id == event_id {
                    self.cursor_event_id = event_id;
                    self.sent_events_total = self.sent_events_total.saturating_add(1);
                    if let Err(e) = self
                        .in_flight
                        .set_cursor_event_id(self.cursor_event_id)
                        .await
                    {
                        warn!("failed to persist cursor: {e:#}");
                    }
                    self.inflight = None;

                    if let Some(end) = self.end_event_id
                        && self.cursor_event_id >= end
                    {
                        info!(
                            "end event_id reached; pausing (cursor={} end={})",
                            self.cursor_event_id, end
                        );
                        self.paused = true;
                        if let Err(e) = self.in_flight.set_paused(true).await {
                            warn!("failed to persist paused state: {e:#}");
                        }
                    }
                }
            }
        }
    }

    async fn on_control_command(&mut self, cmd: ControlCommand) {
        match cmd {
            ControlCommand::SetPaused { paused } => {
                self.paused = paused;
                if let Err(e) = self.in_flight.set_paused(paused).await {
                    warn!("failed to persist paused state: {e:#}");
                }
            }
            ControlCommand::SetSpeed {
                proofs_per_window,
                window_ms,
            } => {
                let proofs_per_window = proofs_per_window.max(1);
                let window_ms = window_ms.max(1);
                self.proofs_per_window = proofs_per_window;
                self.window = Duration::from_millis(window_ms);
                self.window_tokens_remaining = proofs_per_window;
                self.window_started_at = tokio::time::Instant::now();
                if let Err(e) = self.in_flight.set_speed(proofs_per_window, window_ms).await {
                    warn!("failed to persist speed: {e:#}");
                }
            }
            ControlCommand::SetCursorAndEnd {
                cursor_event_id,
                end_event_id,
            } => {
                // Deterministic behavior: pause, reset, then let the operator start.
                self.paused = true;
                self.end_event_id = end_event_id;
                self.cursor_event_id = cursor_event_id;
                self.sent_events_total = 0;
                self.event_buf.clear();
                self.inflight = None;
                if let Err(e) = self
                    .in_flight
                    .set_cursor_and_end(cursor_event_id, end_event_id)
                    .await
                {
                    warn!("failed to persist cursor/end: {e:#}");
                }
                if let Err(e) = self.in_flight.set_paused(true).await {
                    warn!("failed to persist paused state: {e:#}");
                }
            }
            ControlCommand::SetSendMode { send_mode } => {
                self.send_mode = send_mode;
                if let Err(e) = self.in_flight.set_send_mode(send_mode).await {
                    warn!("failed to persist send mode: {e:#}");
                }
            }
        }
    }

    async fn maybe_refill_events(&mut self) {
        if self.paused {
            return;
        }
        if let Some(end) = self.end_event_id
            && self.cursor_event_id >= end
        {
            return;
        }
        if self.event_buf.len() > 10_000 {
            return;
        }
        if self.inflight.is_some() {
            return;
        }
        if !self.event_buf.is_empty() {
            return;
        }

        let limit = EVENTS_READ_LIMIT;
        let resp = match self.archive.read_events(self.cursor_event_id, limit).await {
            Ok(r) => r,
            Err(e) => {
                self.last_error = Some(format!("{e:#}"));
                warn!("read_events failed: {e:#}");
                return;
            }
        };
        self.last_error = None;

        self.first_event_id = resp.first_event_id;
        self.last_event_id = resp.last_event_id;

        // If the operator sets a cursor that is ahead of the current event-log head,
        // we would otherwise appear "stuck" forever (there can never be any events > cursor).
        //
        // Keep behavior consistent with the retention-gap handling below: clamp the cursor
        // back into the valid range.
        if self.cursor_event_id > resp.last_event_id {
            warn!(
                "cursor ahead of event log head; clamping: cursor={} last_event_id={}",
                self.cursor_event_id, resp.last_event_id
            );
            self.cursor_event_id = resp.last_event_id;
            let _ = self
                .in_flight
                .set_cursor_event_id(self.cursor_event_id)
                .await;
            return;
        }

        // Handle retention gaps (best effort): if our cursor is behind the retained range,
        // jump forward so we can keep working.
        if self.cursor_event_id > 0
            && resp.first_event_id > 0
            && self.cursor_event_id < resp.first_event_id.saturating_sub(1)
        {
            warn!(
                "event retention gap detected: cursor={} first_event_id={}",
                self.cursor_event_id, resp.first_event_id
            );
            self.cursor_event_id = resp.first_event_id.saturating_sub(1);
            let _ = self
                .in_flight
                .set_cursor_event_id(self.cursor_event_id)
                .await;
        }

        for ev in resp.events {
            if let Some(end) = self.end_event_id
                && ev.event_id > end
            {
                break;
            }
            self.event_buf.push_back(ev);
        }
    }

    fn refill_send_budget(&mut self) {
        if self.window.is_zero() {
            return;
        }
        let now = tokio::time::Instant::now();
        let elapsed = now.saturating_duration_since(self.window_started_at);
        let window_ms = self.window.as_millis();
        if window_ms == 0 {
            return;
        }
        let elapsed_ms = elapsed.as_millis();
        if elapsed_ms < window_ms {
            return;
        }

        let remainder_ms = (elapsed_ms % window_ms) as u64;
        self.window_started_at = now - Duration::from_millis(remainder_ms);
        self.window_tokens_remaining = self.proofs_per_window;
    }

    async fn maybe_send_next(&mut self) {
        self.refill_send_budget();

        if self.paused {
            return;
        }
        if self.window_tokens_remaining == 0 {
            return;
        }
        if self.inflight.is_some() {
            return;
        }

        if let Some(end) = self.end_event_id
            && self.cursor_event_id >= end
        {
            self.paused = true;
            if let Err(e) = self.in_flight.set_paused(true).await {
                warn!("failed to persist paused state: {e:#}");
            }
            return;
        }

        if self.event_buf.is_empty() {
            self.maybe_refill_events().await;
        }
        let Some(ev) = self.event_buf.pop_front() else {
            return;
        };
        let msg_bytes = match self.send_mode {
            SendMode::Announcement => {
                let Some(vdf) = ev.vdf_info.clone() else {
                    warn!(
                        "missing vdf_info for announcement mode (event_id={})",
                        ev.event_id
                    );
                    self.cursor_event_id = ev.event_id;
                    let _ = self
                        .in_flight
                        .set_cursor_event_id(self.cursor_event_id)
                        .await;
                    return;
                };
                match build_new_compact_vdf_message(&ev, &vdf) {
                    Ok(b) => Arc::new(b),
                    Err(e) => {
                        warn!(
                            "build new_compact_vdf failed (event_id={}): {e:#}",
                            ev.event_id
                        );
                        self.cursor_event_id = ev.event_id;
                        let _ = self
                            .in_flight
                            .set_cursor_event_id(self.cursor_event_id)
                            .await;
                        return;
                    }
                }
            }
            SendMode::RespondOnly => {
                match build_respond_compact_vdf_message(&ev.respond_compact_vdf) {
                    Ok(b) => Arc::new(b),
                    Err(e) => {
                        warn!(
                            "build respond_compact_vdf failed (event_id={}): {e:#}",
                            ev.event_id
                        );
                        self.cursor_event_id = ev.event_id;
                        let _ = self
                            .in_flight
                            .set_cursor_event_id(self.cursor_event_id)
                            .await;
                        return;
                    }
                }
            }
        };

        let mut enqueued = 0usize;
        for addr in &self.connected_peers {
            let Some(tx) = self.peers.get(addr) else {
                continue;
            };
            // Channel is bounded, but we send at a very low rate (<= 1.5 msg/sec).
            // If a peer is too slow, dropping is acceptable for this "best-effort injector".
            if tx
                .try_send(OutboundMessage {
                    bytes: msg_bytes.clone(),
                    event_id: Some(ev.event_id),
                })
                .is_ok()
            {
                enqueued += 1;
            }
        }

        // If all outbound channels are saturated, don't stall the whole injector by setting
        // `inflight` for an event that will never be sent. Put the event back and retry later.
        if enqueued == 0 {
            warn!(
                "no peers accepted outbound message (channels full); retrying later (event_id={})",
                ev.event_id
            );
            self.event_buf.push_front(ev);
            return;
        }

        self.inflight = Some(Inflight {
            event_id: ev.event_id,
            created_at: tokio::time::Instant::now(),
            msg_bytes,
        });
        self.window_tokens_remaining = self.window_tokens_remaining.saturating_sub(1);
    }

    async fn maybe_retry_inflight(&mut self) {
        let Some(inflight) = self.inflight.as_mut() else {
            return;
        };
        // If we didn't observe any successful socket write after a while (e.g. peers down),
        // re-try sending the same event.
        if inflight.created_at.elapsed() < Duration::from_secs(5) {
            return;
        }
        warn!(
            "inflight event not acked after 5s (event_id={}); retrying",
            inflight.event_id
        );
        inflight.created_at = tokio::time::Instant::now();
        for addr in &self.connected_peers {
            let Some(tx) = self.peers.get(addr) else {
                continue;
            };
            let _ = tx.try_send(OutboundMessage {
                bytes: inflight.msg_bytes.clone(),
                event_id: Some(inflight.event_id),
            });
        }
    }

    fn publish_snapshot(&self) {
        let window_ms: u64 = self.window.as_millis().try_into().unwrap_or(u64::MAX);
        let events_per_minute = if window_ms == 0 {
            0
        } else {
            // Conservative integer approximation for UI display.
            (self.proofs_per_window as u64)
                .saturating_mul(60_000)
                .saturating_div(window_ms)
        };
        let mut peer_statuses: Vec<PeerStatusSnapshot> = self
            .peers
            .keys()
            .map(|addr| PeerStatusSnapshot {
                addr: addr.to_string(),
                connected: self.connected_peers.contains(addr),
                reported_peers_known: self.remote_peer_counts.contains_key(addr),
                reported_peer_count: self.remote_peer_counts.get(addr).copied().unwrap_or(0),
            })
            .collect();
        peer_statuses.sort_by(|a, b| a.addr.cmp(&b.addr));

        let snap = InjectorSnapshot {
            started_ts: self.started_ts,
            updated_ts: unix_ts(),
            events_per_minute,
            proofs_per_window: self.proofs_per_window,
            window_ms,
            window_tokens_remaining: self.window_tokens_remaining,
            targets_total: self.peers.len(),
            connected_peers: self.connected_peers.len(),
            paused: self.paused,
            end_event_id: self.end_event_id,
            send_mode: self.send_mode,
            cursor_event_id: self.cursor_event_id,
            sent_events_total: self.sent_events_total,
            first_event_id: self.first_event_id,
            last_event_id: self.last_event_id,
            peer_statuses,
            backlog_events: self.event_buf.len(),
            inflight_event_id: self.inflight.as_ref().map(|i| i.event_id),
            last_error: self.last_error.clone(),
        };
        self.snapshot_tx.send_replace(snap);
    }
}

fn spawn_peer_task(
    addr: SocketAddr,
    config: Config,
    mut outbound_rx: mpsc::Receiver<OutboundMessage>,
    event_tx: mpsc::Sender<PeerEvent>,
    proof_provider: Arc<dyn ProofProvider>,
    proof_metrics: Arc<ProofServingMetrics>,
) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(200);
        loop {
            let res = PeerSession::run(
                addr,
                config.clone(),
                &mut outbound_rx,
                event_tx.clone(),
                proof_provider.clone(),
                proof_metrics.clone(),
            )
            .await;

            match res {
                Ok(()) => {
                    // Session ended; retry.
                    backoff = Duration::from_millis(200);
                }
                Err(e) => {
                    warn!("peer session error ({addr}): {e:#}");
                    let _ = event_tx.send(PeerEvent::Disconnected { addr }).await;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            }

            tokio::time::sleep(backoff).await;
        }
    });
}

fn unix_ts() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
