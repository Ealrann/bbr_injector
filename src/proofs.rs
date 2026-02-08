use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use chia_traits::Streamable;
use serde::Serialize;
use tokio::sync::{Mutex, Semaphore};

use crate::archive_db::ArchiveDb;
use crate::metrics::{Counter, GaugeU64, Timer, TimerSnapshot};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompactionKey {
    pub height: u32,
    pub header_hash: chia_protocol::Bytes32,
    pub field_vdf: u8,
    pub vdf_info_bytes: Vec<u8>,
}

impl CompactionKey {
    pub fn from_request(req: &chia_protocol::RequestCompactVDF) -> anyhow::Result<Self> {
        Ok(Self {
            height: req.height,
            header_hash: req.header_hash,
            field_vdf: req.field_vdf,
            vdf_info_bytes: req.vdf_info.to_bytes()?,
        })
    }
}

#[async_trait::async_trait]
pub trait ProofProvider: Send + Sync {
    /// Returns Streamable bytes for `RespondCompactVDF`.
    async fn fetch_respond_compact_vdf(
        &self,
        request_compact_vdf_bytes: &[u8],
    ) -> anyhow::Result<Option<Vec<u8>>>;
}

#[derive(Debug)]
struct ProofCache {
    max_entries: usize,
    map: HashMap<CompactionKey, Arc<Vec<u8>>>,
    order: VecDeque<CompactionKey>,
}

impl ProofCache {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &CompactionKey) -> Option<Arc<Vec<u8>>> {
        self.map.get(key).cloned()
    }

    fn insert(&mut self, key: CompactionKey, value: Arc<Vec<u8>>) {
        if self.map.contains_key(&key) {
            self.map.insert(key, value);
            return;
        }

        self.map.insert(key.clone(), value);
        self.order.push_back(key);

        while self.map.len() > self.max_entries {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct ProofServingMetrics {
    pub requests_total: Counter,

    pub cache_hits_total: Counter,
    pub cache_misses_total: Counter,

    pub fetch_inflight: GaugeU64,
    pub fetch_ok_total: Counter,
    pub fetch_not_found_total: Counter,
    pub fetch_err_total: Counter,
    pub announcement_request_match_total: Counter,
    pub announcement_request_miss_total: Counter,

    pub respond_send_ok_total: Counter,
    pub respond_send_err_total: Counter,

    pub permit_wait_ms: Timer,
    pub fetch_db_ms: Timer,
    pub fetch_total_ms: Timer,
    pub announcement_to_request_ms: Timer,
    pub respond_send_ms: Timer,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProofServingSnapshot {
    pub requests_total: u64,
    pub cache_hits_total: u64,
    pub cache_misses_total: u64,

    pub fetch_inflight: u64,
    pub fetch_ok_total: u64,
    pub fetch_not_found_total: u64,
    pub fetch_err_total: u64,
    pub announcement_request_match_total: u64,
    pub announcement_request_miss_total: u64,

    pub respond_send_ok_total: u64,
    pub respond_send_err_total: u64,

    pub permit_wait_ms: TimerSnapshot,
    pub fetch_db_ms: TimerSnapshot,
    pub fetch_total_ms: TimerSnapshot,
    pub announcement_to_request_ms: TimerSnapshot,
    pub respond_send_ms: TimerSnapshot,
}

impl ProofServingMetrics {
    pub fn snapshot(&self) -> ProofServingSnapshot {
        ProofServingSnapshot {
            requests_total: self.requests_total.get(),
            cache_hits_total: self.cache_hits_total.get(),
            cache_misses_total: self.cache_misses_total.get(),

            fetch_inflight: self.fetch_inflight.get(),
            fetch_ok_total: self.fetch_ok_total.get(),
            fetch_not_found_total: self.fetch_not_found_total.get(),
            fetch_err_total: self.fetch_err_total.get(),
            announcement_request_match_total: self.announcement_request_match_total.get(),
            announcement_request_miss_total: self.announcement_request_miss_total.get(),

            respond_send_ok_total: self.respond_send_ok_total.get(),
            respond_send_err_total: self.respond_send_err_total.get(),

            permit_wait_ms: self.permit_wait_ms.snapshot(),
            fetch_db_ms: self.fetch_db_ms.snapshot(),
            fetch_total_ms: self.fetch_total_ms.snapshot(),
            announcement_to_request_ms: self.announcement_to_request_ms.snapshot(),
            respond_send_ms: self.respond_send_ms.snapshot(),
        }
    }
}

pub struct ArchiveProofProvider {
    archive: Arc<ArchiveDb>,
    cache: Mutex<ProofCache>,
    fetch_sem: Semaphore,
    metrics: Arc<ProofServingMetrics>,
}

impl ArchiveProofProvider {
    pub fn new(
        archive: Arc<ArchiveDb>,
        max_cache_entries: usize,
        metrics: Arc<ProofServingMetrics>,
    ) -> Self {
        Self {
            archive,
            cache: Mutex::new(ProofCache::new(max_cache_entries)),
            fetch_sem: Semaphore::new(64),
            metrics,
        }
    }
}

#[async_trait::async_trait]
impl ProofProvider for ArchiveProofProvider {
    async fn fetch_respond_compact_vdf(
        &self,
        request_compact_vdf_bytes: &[u8],
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let total_start = Instant::now();
        self.metrics.requests_total.inc();

        let req = chia_protocol::RequestCompactVDF::from_bytes(request_compact_vdf_bytes)?;
        let key = CompactionKey::from_request(&req)?;

        {
            let mut cache = self.cache.lock().await;
            if let Some(hit) = cache.get(&key) {
                self.metrics.cache_hits_total.inc();
                self.metrics.fetch_total_ms.record(total_start.elapsed());
                return Ok(Some((*hit).clone()));
            }
        }
        self.metrics.cache_misses_total.inc();

        let permit_start = Instant::now();
        let _permit = self.fetch_sem.acquire().await?;
        self.metrics.permit_wait_ms.record(permit_start.elapsed());
        let _inflight = self.metrics.fetch_inflight.guard_inc();

        let db_start = Instant::now();
        let bytes = self.archive.fetch_proof(request_compact_vdf_bytes).await;
        self.metrics.fetch_db_ms.record(db_start.elapsed());

        let bytes = match bytes {
            Ok(Some(b)) => {
                self.metrics.fetch_ok_total.inc();
                Ok(Some(b))
            }
            Ok(None) => {
                self.metrics.fetch_not_found_total.inc();
                Ok(None)
            }
            Err(e) => {
                self.metrics.fetch_err_total.inc();
                Err(e)
            }
        }?;

        if let Some(b) = &bytes {
            let mut cache = self.cache.lock().await;
            cache.insert(key, Arc::new(b.clone()));
        }

        self.metrics.fetch_total_ms.record(total_start.elapsed());
        Ok(bytes)
    }
}
