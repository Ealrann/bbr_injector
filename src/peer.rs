use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chia_protocol::{
    Handshake, Message, NodeType, ProtocolMessageTypes, RequestCompactVDF, RequestPeers,
    RespondPeers,
};
use chia_traits::Streamable;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::config::Config;
use crate::proofs::{CompactionKey, ProofProvider, ProofServingMetrics};
use crate::ws;

#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub bytes: Arc<Vec<u8>>,
    pub event_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum PeerEvent {
    Connected { addr: SocketAddr },
    Disconnected { addr: SocketAddr },
    PeerListReceived { addr: SocketAddr, peer_count: usize },
    AnnouncementSent { event_id: u64 },
}

pub struct PeerSession;

#[derive(Debug, Clone)]
struct PendingAnnouncement {
    sent_at: Instant,
    seq: u64,
}

const ANNOUNCEMENT_TRACK_TTL: Duration = Duration::from_secs(120);
const MAX_PENDING_ANNOUNCEMENTS: usize = 50_000;

impl PeerSession {
    pub async fn run(
        addr: SocketAddr,
        config: Config,
        outbound_rx: &mut mpsc::Receiver<OutboundMessage>,
        event_tx: mpsc::Sender<PeerEvent>,
        proof_provider: Arc<dyn ProofProvider>,
        proof_metrics: Arc<ProofServingMetrics>,
    ) -> anyhow::Result<()> {
        let connector = ws::connector_from_paths(&config.tls.cert_path, &config.tls.key_path)
            .context("load TLS cert/key")?;
        let ws = ws::connect_wss(addr, connector, Duration::from_secs(5)).await?;

        let (mut sink, mut stream) = ws.split();

        let handshake = Handshake {
            network_id: config.network_id.clone(),
            protocol_version: config
                .protocol_version
                .clone()
                .unwrap_or_else(|| "0.0.36".to_string()),
            software_version: format!("bbr-injector/{}", env!("CARGO_PKG_VERSION")),
            server_port: 0,
            node_type: NodeType::FullNode,
            capabilities: vec![
                (1, "1".to_string()),
                (2, "1".to_string()),
                (3, "1".to_string()),
                (5, "1".to_string()),
            ],
        };

        ws::send_streamable_message(&mut sink, ProtocolMessageTypes::Handshake, None, &handshake)
            .await
            .context("send handshake")?;

        // Await peer handshake.
        let remote_handshake = {
            let msg = timeout(
                Duration::from_secs(5),
                ws::read_next_chia_message(&mut stream),
            )
            .await
            .map_err(|_| anyhow::anyhow!("handshake recv timeout"))??;
            let msg = msg.context("peer closed before handshake")?;
            if msg.msg_type != ProtocolMessageTypes::Handshake {
                return Err(anyhow::anyhow!("peer did not respond with Handshake"));
            }
            Handshake::from_bytes(&msg.data).context("parse peer handshake")?
        };

        if remote_handshake.network_id != config.network_id {
            return Err(anyhow::anyhow!(
                "network_id mismatch: {}",
                remote_handshake.network_id
            ));
        }
        if remote_handshake.node_type != NodeType::FullNode {
            return Err(anyhow::anyhow!("peer is not a full node"));
        }

        event_tx.send(PeerEvent::Connected { addr }).await.ok();

        // Be protocol-friendly: request peers (we ignore the response).
        let _ = ws::send_streamable_message(
            &mut sink,
            ProtocolMessageTypes::RequestPeers,
            None,
            &RequestPeers {},
        )
        .await;

        let mut pending_announcements = HashMap::<CompactionKey, PendingAnnouncement>::new();
        let mut pending_order = VecDeque::<(CompactionKey, u64)>::new();
        let mut pending_seq: u64 = 1;

        loop {
            tokio::select! {
                Some(out) = outbound_rx.recv() => {
                    let send_res = timeout(Duration::from_secs(5), ws::send_raw_bytes(&mut sink, out.bytes.as_ref()))
                        .await
                        .map_err(|_| anyhow::anyhow!("ws send timeout"))
                        .and_then(|r| r);
                    if let Err(_e) = send_res {
                        event_tx.send(PeerEvent::Disconnected { addr }).await.ok();
                        return Ok(());
                    }

                    let now = Instant::now();
                    match parse_compaction_key_from_announcement(out.bytes.as_ref()) {
                        Ok(Some(key)) => {
                            remember_pending_announcement(
                                &mut pending_announcements,
                                &mut pending_order,
                                &mut pending_seq,
                                key,
                                now,
                            );
                        }
                        Ok(None) => {}
                        Err(e) => {
                            debug!("failed to parse outbound announcement key ({addr}): {e:#}");
                        }
                    }
                    cleanup_pending_announcements(
                        &mut pending_announcements,
                        &mut pending_order,
                        now,
                    );

                    if let Some(event_id) = out.event_id {
                        event_tx.send(PeerEvent::AnnouncementSent { event_id }).await.ok();
                    }
                }
                msg = ws::read_next_chia_message(&mut stream) => {
                    match msg {
                        Ok(Some(msg)) => {
                            if let Err(e) = handle_inbound(
                                addr,
                                &mut sink,
                                &event_tx,
                                &proof_provider,
                                &proof_metrics,
                                &mut pending_announcements,
                                &mut pending_order,
                                msg,
                            )
                            .await
                            {
                                warn!("peer message handling error ({addr}): {e:#}");
                            }
                        }
                        Ok(None) => {
                            event_tx.send(PeerEvent::Disconnected { addr }).await.ok();
                            return Ok(());
                        }
                        Err(e) => {
                            warn!("peer recv error ({addr}): {e}");
                            event_tx.send(PeerEvent::Disconnected { addr }).await.ok();
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

async fn handle_inbound(
    addr: SocketAddr,
    sink: &mut ws::WsSink,
    tx: &mpsc::Sender<PeerEvent>,
    proof_provider: &Arc<dyn ProofProvider>,
    proof_metrics: &Arc<ProofServingMetrics>,
    pending_announcements: &mut HashMap<CompactionKey, PendingAnnouncement>,
    pending_order: &mut VecDeque<(CompactionKey, u64)>,
    msg: Message,
) -> anyhow::Result<()> {
    match msg.msg_type {
        ProtocolMessageTypes::RespondPeers => {
            let rp = RespondPeers::from_bytes(&msg.data).context("parse RespondPeers")?;
            let peer_count = rp.peer_list.len();
            tx.send(PeerEvent::PeerListReceived { addr, peer_count })
                .await
                .ok();
        }
        ProtocolMessageTypes::RequestPeers => {
            // Respond with an empty list; we do not want to be an introducer, but we also
            // want to be protocol-friendly and avoid timeouts.
            let rp = RespondPeers {
                peer_list: Vec::new(),
            };
            ws::send_streamable_message(sink, ProtocolMessageTypes::RespondPeers, msg.id, &rp)
                .await
                .ok();
        }
        ProtocolMessageTypes::RequestCompactVDF => {
            let req =
                RequestCompactVDF::from_bytes(&msg.data).context("parse RequestCompactVDF")?;
            let now = Instant::now();
            cleanup_pending_announcements(pending_announcements, pending_order, now);
            let key = CompactionKey::from_request(&req).context("key from RequestCompactVDF")?;
            if let Some(pending) = pending_announcements.remove(&key) {
                proof_metrics.announcement_request_match_total.inc();
                proof_metrics
                    .announcement_to_request_ms
                    .record(now.saturating_duration_since(pending.sent_at));
            } else {
                proof_metrics.announcement_request_miss_total.inc();
            }

            if let Some(resp_bytes) = proof_provider
                .fetch_respond_compact_vdf(&msg.data)
                .await
                .context("fetch respond_compact_vdf")?
            {
                let out = Message {
                    msg_type: ProtocolMessageTypes::RespondCompactVDF,
                    id: msg.id,
                    data: resp_bytes.into(),
                };
                let send_start = Instant::now();
                let res = timeout(Duration::from_secs(5), ws::send_chia_message(sink, &out))
                    .await
                    .map_err(|_| anyhow::anyhow!("ws send timeout"))
                    .and_then(|r| r);
                proof_metrics.respond_send_ms.record(send_start.elapsed());
                match res {
                    Ok(()) => proof_metrics.respond_send_ok_total.inc(),
                    Err(e) => {
                        proof_metrics.respond_send_err_total.inc();
                        debug!("respond_compact_vdf send failed ({addr}): {e}");
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_compaction_key_from_announcement(bytes: &[u8]) -> anyhow::Result<Option<CompactionKey>> {
    let msg = Message::from_bytes(bytes).context("parse outbound message")?;
    if msg.msg_type != ProtocolMessageTypes::NewCompactVDF {
        return Ok(None);
    }

    let announcement =
        chia_protocol::NewCompactVDF::from_bytes(&msg.data).context("parse NewCompactVDF")?;
    Ok(Some(CompactionKey {
        height: announcement.height,
        header_hash: announcement.header_hash,
        field_vdf: announcement.field_vdf,
        vdf_info_bytes: announcement.vdf_info.to_bytes()?,
    }))
}

fn remember_pending_announcement(
    pending_announcements: &mut HashMap<CompactionKey, PendingAnnouncement>,
    pending_order: &mut VecDeque<(CompactionKey, u64)>,
    pending_seq: &mut u64,
    key: CompactionKey,
    now: Instant,
) {
    let seq = *pending_seq;
    *pending_seq = pending_seq.wrapping_add(1);
    pending_announcements.insert(
        key.clone(),
        PendingAnnouncement {
            sent_at: now,
            seq,
        },
    );
    pending_order.push_back((key, seq));
}

fn cleanup_pending_announcements(
    pending_announcements: &mut HashMap<CompactionKey, PendingAnnouncement>,
    pending_order: &mut VecDeque<(CompactionKey, u64)>,
    now: Instant,
) {
    loop {
        let Some((key, seq)) = pending_order.front().cloned() else {
            break;
        };

        let Some(entry) = pending_announcements.get(&key) else {
            pending_order.pop_front();
            continue;
        };
        if entry.seq != seq {
            pending_order.pop_front();
            continue;
        }

        let expired = now.saturating_duration_since(entry.sent_at) >= ANNOUNCEMENT_TRACK_TTL;
        let over_limit = pending_announcements.len() > MAX_PENDING_ANNOUNCEMENTS;
        if expired || over_limit {
            pending_order.pop_front();
            let _ = pending_announcements.remove(&key);
            continue;
        }
        break;
    }
}
