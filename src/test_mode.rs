use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::archive_db::{ArchiveDb, CompactionEvent};
use crate::chia_messages::{build_new_compact_vdf_message, resolve_socket_addrs};
use crate::config::Config;
use crate::peer::{OutboundMessage, PeerEvent, PeerSession};
use crate::proofs::{ArchiveProofProvider, ProofServingMetrics};
use crate::toy_debug_client::{P2pMetrics, ToyDebugClient};

#[derive(Debug, Clone)]
pub struct TestArgs {
    pub toy_a_base_url: String,
    pub toy_b_base_url: Option<String>,
    pub target_peer: Option<String>,
    pub proofs_to_inject: usize,
}

pub async fn run_test(config: Config, args: TestArgs) -> anyhow::Result<()> {
    let toy_a = ToyDebugClient::new(&args.toy_a_base_url, 5)?;
    let toy_b = match &args.toy_b_base_url {
        Some(url) => Some(ToyDebugClient::new(url, 5)?),
        None => None,
    };

    let summary_a = toy_a.summary().await.context("toy A summary")?;
    let peak_a = summary_a
        .peak_height
        .context("toy A has no peak height (not synced?)")?;
    let min_height = peak_a.saturating_sub(5);

    let (before_a, before_b) = (
        toy_a.p2p_metrics().await.context("toy A metrics")?,
        match &toy_b {
            Some(b) => Some(b.p2p_metrics().await.context("toy B metrics")?),
            None => None,
        },
    );

    info!(
        "Toy A peak_height={peak_a} (min_height_for_new_compact_vdf={min_height}) compact={} uncompact={} ({:.2}%)",
        summary_a.compact_blocks,
        summary_a.uncompact_blocks,
        summary_a.percent_compact * 100.0
    );
    info!("Toy A metrics before: {}", fmt_metrics_brief(&before_a));
    if let Some(b) = &before_b {
        info!("Toy B metrics before: {}", fmt_metrics_brief(b));
    }

    let archive = Arc::new(ArchiveDb::new(&config.archive)?);
    let proof_metrics = Arc::new(ProofServingMetrics::default());
    let proof_provider = Arc::new(ArchiveProofProvider::new(
        archive.clone(),
        50_000,
        proof_metrics.clone(),
    ));

    let (events, found_after_event_id) = find_events_to_inject(
        archive.as_ref(),
        &toy_a,
        toy_b.as_ref(),
        min_height,
        args.proofs_to_inject,
    )
    .await?;

    info!(
        "Selected {} event(s) to inject (starting_scan_after_event_id={})",
        events.len(),
        found_after_event_id
    );
    for ev in &events {
        info!(
            "  event_id={} height={} field_vdf={} header_hash={}",
            ev.event_id,
            ev.height,
            ev.field_vdf,
            hex::encode(&ev.header_hash)
        );
    }

    let target = match &args.target_peer {
        Some(s) => s.clone(),
        None => config
            .peers
            .targets
            .first()
            .cloned()
            .context("no peers.targets in config")?,
    };
    let target_addr = resolve_target_addr(&target).await?;
    info!("Injecting into peer {target} -> {target_addr}");

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundMessage>(1024);
    let (peer_evt_tx, mut peer_evt_rx) = mpsc::channel::<PeerEvent>(1024);

    let cfg_for_peer = config.clone();
    let proof_provider_for_peer = proof_provider.clone();
    let proof_metrics_for_peer = proof_metrics.clone();
    let peer_task = tokio::spawn(async move {
        PeerSession::run(
            target_addr,
            cfg_for_peer,
            &mut outbound_rx,
            peer_evt_tx,
            proof_provider_for_peer,
            proof_metrics_for_peer,
        )
        .await
    });

    wait_for_connected(&mut peer_evt_rx, target_addr).await?;

    for ev in &events {
        let Some(vdf) = &ev.vdf_info else {
            continue;
        };
        let msg_bytes = Arc::new(build_new_compact_vdf_message(ev, vdf)?);
        outbound_tx
            .send(OutboundMessage {
                bytes: msg_bytes,
                event_id: Some(ev.event_id),
            })
            .await
            .context("enqueue outbound announcement")?;

        wait_for_announcement_sent(&mut peer_evt_rx, ev.event_id).await?;
        info!("announcement sent (event_id={})", ev.event_id);
    }

    // Give the network time to do request/response. Prefer waiting for evidence over a fixed sleep.
    let expected_a_replace_ok_min = before_a.respond_compact_vdf_replace_ok + 1;
    let expected_b_replace_ok_min = before_b
        .as_ref()
        .map(|m| m.respond_compact_vdf_replace_ok + 1);

    let mut after_a = toy_a.p2p_metrics().await.context("toy A metrics poll")?;
    let mut after_b = match &toy_b {
        Some(b) => Some(b.p2p_metrics().await.context("toy B metrics poll")?),
        None => None,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let a_ok = after_a.respond_compact_vdf_replace_ok >= expected_a_replace_ok_min;
        let b_ok = expected_b_replace_ok_min
            .zip(after_b.as_ref())
            .map(|(exp, cur)| cur.respond_compact_vdf_replace_ok >= exp)
            .unwrap_or(true);
        if a_ok && b_ok {
            break;
        }

        if tokio::time::Instant::now() >= deadline {
            warn!("timeout waiting for replace_ok evidence (toy A expected >= {expected_a_replace_ok_min}, toy B expected >= {expected_b_replace_ok_min:?})");
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;

        after_a = toy_a.p2p_metrics().await.context("toy A metrics poll")?;
        after_b = match &toy_b {
            Some(b) => Some(b.p2p_metrics().await.context("toy B metrics poll")?),
            None => None,
        };
    }

    info!("Toy A metrics after: {}", fmt_metrics_brief(&after_a));
    let delta_a = delta_metrics(&before_a, &after_a);
    info!("Toy A metrics delta: {}", fmt_metrics_brief(&delta_a));
    if let (Some(before_b), Some(after_b)) = (before_b.as_ref(), after_b.as_ref()) {
        info!("Toy B metrics after: {}", fmt_metrics_brief(after_b));
        let delta_b = delta_metrics(before_b, after_b);
        info!("Toy B metrics delta: {}", fmt_metrics_brief(&delta_b));

        let a_proof_served_ok = delta_a.request_compact_vdf_responded > 0;
        let b_applied_ok = delta_b.respond_compact_vdf_replace_ok > 0;

        if delta_a.respond_compact_vdf_replace_ok > 0 && a_proof_served_ok && b_applied_ok {
            info!(
                "TEST PASS: injected={} applied_on_A={} applied_on_B={} (A served_proofs={})",
                events.len(),
                delta_a.respond_compact_vdf_replace_ok,
                delta_b.respond_compact_vdf_replace_ok,
                delta_a.request_compact_vdf_responded,
            );
        } else {
            warn!(
                "TEST FAIL: injected={} applied_on_A={} applied_on_B={} (A served_proofs={})",
                events.len(),
                delta_a.respond_compact_vdf_replace_ok,
                delta_b.respond_compact_vdf_replace_ok,
                delta_a.request_compact_vdf_responded,
            );
            return Err(anyhow::anyhow!("test failed (see logs for metrics deltas)"));
        }
    } else {
        if delta_a.respond_compact_vdf_replace_ok > 0 {
            info!(
                "TEST PASS: injected={} applied_on_A={}",
                events.len(),
                delta_a.respond_compact_vdf_replace_ok
            );
        } else {
            warn!(
                "TEST FAIL: injected={} applied_on_A={}",
                events.len(),
                delta_a.respond_compact_vdf_replace_ok
            );
            return Err(anyhow::anyhow!("test failed (no proofs applied on toy A)"));
        }
    }

    peer_task.abort();
    Ok(())
}

async fn resolve_target_addr(target: &str) -> anyhow::Result<SocketAddr> {
    let mut addrs = resolve_socket_addrs(target)
        .await
        .with_context(|| format!("resolve target peer {target}"))?;
    // Prefer IPv4 for ergonomics in typical LAN setups.
    addrs.sort_by_key(|a| !a.is_ipv4());
    addrs
        .into_iter()
        .next()
        .context("target peer resolved to 0 addresses")
}

async fn wait_for_connected(
    peer_evt_rx: &mut mpsc::Receiver<PeerEvent>,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(anyhow::anyhow!("timeout waiting for peer connect: {addr}"));
        }
        let remaining = deadline - now;
        let evt = tokio::time::timeout(remaining, peer_evt_rx.recv())
            .await
            .context("wait connected timeout")?
            .context("peer task ended before connect")?;
        if matches!(evt, PeerEvent::Connected { addr: a } if a == addr) {
            return Ok(());
        }
    }
}

async fn wait_for_announcement_sent(
    peer_evt_rx: &mut mpsc::Receiver<PeerEvent>,
    event_id: u64,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(anyhow::anyhow!(
                "timeout waiting for announcement sent (event_id={event_id})"
            ));
        }
        let remaining = deadline - now;
        let evt = tokio::time::timeout(remaining, peer_evt_rx.recv())
            .await
            .context("wait announcement timeout")?
            .context("peer task ended while waiting for announcement")?;
        if matches!(evt, PeerEvent::AnnouncementSent { event_id: id } if id == event_id) {
            return Ok(());
        }
    }
}

async fn find_events_to_inject(
    archive: &ArchiveDb,
    toy_a: &ToyDebugClient,
    toy_b: Option<&ToyDebugClient>,
    max_height: u32,
    count: usize,
) -> anyhow::Result<(Vec<CompactionEvent>, u64)> {
    let head = archive.read_events(0, 1).await.context("read event head")?;
    let first = head.first_event_id;
    let last = head.last_event_id;
    if last == 0 {
        return Err(anyhow::anyhow!("event log is empty"));
    }

    let window = 50_000u64;
    let start_after = last.saturating_sub(window);

    let mut after_event_id = start_after;
    let mut picked: Vec<CompactionEvent> = Vec::new();

    while picked.len() < count {
        let resp = archive
            .read_events(after_event_id, 200)
            .await
            .with_context(|| format!("read_events(after_event_id={after_event_id})"))?;
        if resp.events.is_empty() {
            break;
        }
        for ev in resp.events {
            after_event_id = ev.event_id;
            if ev.height > max_height {
                continue;
            }
            let Some(vdf) = &ev.vdf_info else { continue };
            let a_check = toy_a
                .check_needs_compact_proof(&ev, vdf)
                .await
                .context("toy A check")?;
            if a_check.already_fully_compactified == Some(true) {
                continue;
            }
            if !(a_check.block_found && a_check.needs_compact_proof == Some(true)) {
                continue;
            }

            if let Some(b) = toy_b {
                let b_check = b
                    .check_needs_compact_proof(&ev, vdf)
                    .await
                    .context("toy B check")?;
                if b_check.already_fully_compactified == Some(true) {
                    continue;
                }
                if !(b_check.block_found && b_check.needs_compact_proof == Some(true)) {
                    continue;
                }
            }

            picked.push(ev);
            if picked.len() >= count {
                break;
            }
        }

        if after_event_id >= last {
            break;
        }
    }

    if picked.is_empty() {
        warn!(
            "no matching events found in tail window (start_after={start_after}); retrying from start"
        );

        let mut after_event_id = first.saturating_sub(1);
        while picked.len() < count {
            let resp = archive
                .read_events(after_event_id, 200)
                .await
                .with_context(|| format!("read_events(after_event_id={after_event_id})"))?;
            if resp.events.is_empty() {
                break;
            }
            for ev in resp.events {
                after_event_id = ev.event_id;
                if ev.height > max_height {
                    continue;
                }
                let Some(vdf) = &ev.vdf_info else { continue };
                let a_check = toy_a
                    .check_needs_compact_proof(&ev, vdf)
                    .await
                    .context("toy A check")?;
                if a_check.already_fully_compactified == Some(true) {
                    continue;
                }
                if !(a_check.block_found && a_check.needs_compact_proof == Some(true)) {
                    continue;
                }
                if let Some(b) = toy_b {
                    let b_check = b
                        .check_needs_compact_proof(&ev, vdf)
                        .await
                        .context("toy B check")?;
                    if b_check.already_fully_compactified == Some(true) {
                        continue;
                    }
                    if !(b_check.block_found && b_check.needs_compact_proof == Some(true)) {
                        continue;
                    }
                }

                picked.push(ev);
                if picked.len() >= count {
                    break;
                }
            }
            if after_event_id >= last {
                break;
            }
        }
    }

    if picked.is_empty() {
        return Err(anyhow::anyhow!(
            "could not find any events that both toy nodes need (searched tail window and from start)"
        ));
    }

    Ok((picked, start_after))
}

fn delta_metrics(before: &P2pMetrics, after: &P2pMetrics) -> P2pMetrics {
    P2pMetrics {
        new_compact_vdf_rx: after.new_compact_vdf_rx - before.new_compact_vdf_rx,
        new_compact_vdf_too_recent: after.new_compact_vdf_too_recent - before.new_compact_vdf_too_recent,
        new_compact_vdf_already_compact_or_unknown: after.new_compact_vdf_already_compact_or_unknown
            - before.new_compact_vdf_already_compact_or_unknown,
        new_compact_vdf_missing_header_block: after.new_compact_vdf_missing_header_block
            - before.new_compact_vdf_missing_header_block,
        new_compact_vdf_needs_proof: after.new_compact_vdf_needs_proof - before.new_compact_vdf_needs_proof,
        new_compact_vdf_requested_proof: after.new_compact_vdf_requested_proof - before.new_compact_vdf_requested_proof,
        new_compact_vdf_got_proof_response: after.new_compact_vdf_got_proof_response
            - before.new_compact_vdf_got_proof_response,
        request_compact_vdf_rx: after.request_compact_vdf_rx - before.request_compact_vdf_rx,
        request_compact_vdf_responded: after.request_compact_vdf_responded - before.request_compact_vdf_responded,
        request_compact_vdf_missing_or_not_compact: after.request_compact_vdf_missing_or_not_compact
            - before.request_compact_vdf_missing_or_not_compact,
        respond_compact_vdf_rx: after.respond_compact_vdf_rx - before.respond_compact_vdf_rx,
        respond_compact_vdf_rejected: after.respond_compact_vdf_rejected - before.respond_compact_vdf_rejected,
        respond_compact_vdf_already_seen: after.respond_compact_vdf_already_seen - before.respond_compact_vdf_already_seen,
        respond_compact_vdf_replace_failed: after.respond_compact_vdf_replace_failed - before.respond_compact_vdf_replace_failed,
        respond_compact_vdf_replace_ok: after.respond_compact_vdf_replace_ok - before.respond_compact_vdf_replace_ok,
    }
}

fn fmt_metrics_brief(m: &P2pMetrics) -> String {
    format!(
        "new_rx={} requested={} got_proof={} replace_ok={} req_vdf_rx={} req_vdf_rsp={}",
        m.new_compact_vdf_rx,
        m.new_compact_vdf_requested_proof,
        m.new_compact_vdf_got_proof_response,
        m.respond_compact_vdf_replace_ok,
        m.request_compact_vdf_rx,
        m.request_compact_vdf_responded,
    )
}
