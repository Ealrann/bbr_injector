use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::archive_db::{CompactionEvent, VdfInfo};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct P2pMetrics {
    pub new_compact_vdf_rx: u64,
    pub new_compact_vdf_too_recent: u64,
    pub new_compact_vdf_already_compact_or_unknown: u64,
    pub new_compact_vdf_missing_header_block: u64,
    pub new_compact_vdf_needs_proof: u64,
    pub new_compact_vdf_requested_proof: u64,
    pub new_compact_vdf_got_proof_response: u64,

    pub request_compact_vdf_rx: u64,
    pub request_compact_vdf_responded: u64,
    pub request_compact_vdf_missing_or_not_compact: u64,

    pub respond_compact_vdf_rx: u64,
    pub respond_compact_vdf_rejected: u64,
    pub respond_compact_vdf_already_seen: u64,
    pub respond_compact_vdf_replace_failed: u64,
    pub respond_compact_vdf_replace_ok: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompactionSummary {
    pub peak_height: Option<u32>,
    pub compact_blocks: u64,
    pub uncompact_blocks: u64,
    pub percent_compact: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompactVdfCheckResponse {
    pub block_found: bool,
    pub already_fully_compactified: Option<bool>,
    pub needs_compact_proof: Option<bool>,
}

#[derive(Clone)]
pub struct ToyDebugClient {
    base_url: String,
    http: reqwest::Client,
}

impl ToyDebugClient {
    pub fn new(base_url: &str, timeout_secs: u64) -> anyhow::Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("build reqwest client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub async fn summary(&self) -> anyhow::Result<CompactionSummary> {
        let url = format!("{}/debug/v1/compaction/summary", self.base_url);
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .context("GET /debug/v1/compaction/summary")?
            .error_for_status()
            .context("summary status")?;
        resp.json::<CompactionSummary>()
            .await
            .context("parse summary")
    }

    pub async fn p2p_metrics(&self) -> anyhow::Result<P2pMetrics> {
        let url = format!("{}/debug/v1/compact_vdf/p2p_metrics", self.base_url);
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .context("GET /debug/v1/compact_vdf/p2p_metrics")?
            .error_for_status()
            .context("p2p_metrics status")?;
        resp.json::<P2pMetrics>().await.context("parse p2p_metrics")
    }

    pub async fn check_needs_compact_proof(
        &self,
        ev: &CompactionEvent,
        vdf: &VdfInfo,
    ) -> anyhow::Result<CompactVdfCheckResponse> {
        let url = format!("{}/debug/v1/compact_vdf/check", self.base_url);
        let req = CompactVdfCheckRequest::from_event(ev, vdf)?;
        let resp = self
            .http
            .post(url)
            .json(&req)
            .send()
            .await
            .context("POST /debug/v1/compact_vdf/check")?
            .error_for_status()
            .context("check status")?;
        resp.json::<CompactVdfCheckResponse>()
            .await
            .context("parse check response")
    }
}

#[derive(Debug, Clone, Serialize)]
struct CompactVdfCheckRequest {
    height: u32,
    header_hash: String,
    field_vdf: u8,
    vdf_info: CompactVdfCheckVdfInfo,
}

#[derive(Debug, Clone, Serialize)]
struct CompactVdfCheckVdfInfo {
    challenge: String,
    number_of_iterations: u64,
    output: String,
}

impl CompactVdfCheckRequest {
    fn from_event(ev: &CompactionEvent, vdf: &VdfInfo) -> anyhow::Result<Self> {
        Ok(Self {
            height: ev.height,
            header_hash: hex::encode(&ev.header_hash),
            field_vdf: u8::try_from(ev.field_vdf).unwrap_or(0),
            vdf_info: CompactVdfCheckVdfInfo {
                challenge: hex::encode(&vdf.challenge),
                number_of_iterations: vdf.number_of_iterations,
                output: hex::encode(&vdf.output),
            },
        })
    }
}
