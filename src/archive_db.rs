use std::path::PathBuf;

use anyhow::Context;
use serde::Serialize;

use crate::config::ArchiveConfig;

#[derive(Clone)]
pub struct ArchiveDb {
    db_path: PathBuf,
    busy_timeout_ms: u64,
}

impl ArchiveDb {
    pub fn new(cfg: &ArchiveConfig) -> anyhow::Result<Self> {
        Ok(Self {
            db_path: cfg.db_path.clone(),
            busy_timeout_ms: cfg.busy_timeout_ms.max(1),
        })
    }

    pub async fn read_events(
        &self,
        after_event_id: u64,
        limit: u32,
    ) -> anyhow::Result<ReadCompactionEventsResponse> {
        let db_path = self.db_path.clone();
        let busy_timeout_ms = self.busy_timeout_ms;
        let limit: i64 = (limit.max(1)).into();

        tokio::task::spawn_blocking(move || -> anyhow::Result<ReadCompactionEventsResponse> {
            use rusqlite::{Connection, OpenFlags};

            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY;
            let conn = Connection::open_with_flags(&db_path, flags)
                .with_context(|| format!("open archive db {}", db_path.display()))?;
            conn.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))
                .context("set busy_timeout")?;

            let (first_event_id, last_event_id) = conn
                .query_row(
                    "SELECT COALESCE(MIN(event_id), 0), COALESCE(MAX(event_id), 0) FROM compact_proofs",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .context("query bounds")?;

            let mut stmt = conn
                .prepare(
                    "SELECT event_id, ts, height, header_hash, field_vdf, vdf_challenge, vdf_iters, vdf_output, respond_compact_vdf\n\
                     FROM compact_proofs\n\
                     WHERE event_id > ?1\n\
                     ORDER BY event_id ASC\n\
                     LIMIT ?2",
                )
                .context("prepare read_events")?;
            let mut rows = stmt
                .query(rusqlite::params![after_event_id as i64, limit])
                .context("query read_events")?;

            let mut events = Vec::new();
            while let Some(row) = rows.next().context("next row")? {
                let event_id: i64 = row.get(0)?;
                let ts: i64 = row.get(1)?;
                let height: i64 = row.get(2)?;
                let header_hash: Vec<u8> = row.get(3)?;
                let field_vdf: i64 = row.get(4)?;
                let vdf_challenge: Vec<u8> = row.get(5)?;
                let vdf_iters: i64 = row.get(6)?;
                let vdf_output: Vec<u8> = row.get(7)?;
                let respond_compact_vdf: Vec<u8> = row.get(8)?;

                events.push(CompactionEvent {
                    event_id: event_id.max(0) as u64,
                    ts: ts.max(0) as u64,
                    height: height.max(0) as u32,
                    header_hash,
                    field_vdf: field_vdf as i32,
                    vdf_info: Some(VdfInfo {
                        challenge: vdf_challenge,
                        number_of_iterations: vdf_iters.max(0) as u64,
                        output: vdf_output,
                    }),
                    respond_compact_vdf,
                });
            }

            Ok(ReadCompactionEventsResponse {
                first_event_id: first_event_id.max(0) as u64,
                last_event_id: last_event_id.max(0) as u64,
                events,
            })
        })
        .await
        .context("archive db task panicked")?
    }

    /// Returns Streamable bytes for `RespondCompactVDF`, if available.
    pub async fn fetch_proof(
        &self,
        request_compact_vdf_bytes: &[u8],
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let db_path = self.db_path.clone();
        let busy_timeout_ms = self.busy_timeout_ms;
        let req = request_compact_vdf_bytes.to_vec();

        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<u8>>> {
            use rusqlite::{Connection, OpenFlags};

            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY;
            let conn = Connection::open_with_flags(&db_path, flags)
                .with_context(|| format!("open archive db {}", db_path.display()))?;
            conn.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))
                .context("set busy_timeout")?;

            let mut stmt = conn
                .prepare(
                    "SELECT respond_compact_vdf\n\
                     FROM compact_proofs\n\
                     WHERE request_compact_vdf = ?1\n\
                     LIMIT 1",
                )
                .context("prepare fetch_proof")?;

            match stmt.query_row([req], |row| row.get::<_, Vec<u8>>(0)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e).context("query fetch_proof"),
            }
        })
        .await
        .context("archive db task panicked")?
    }
}

// ---- Shared event/proto-ish DTOs (kept compatible with existing injector code) ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum FieldVdf {
    Unspecified = 0,
    CcEosVdf = 1,
    IccEosVdf = 2,
    CcSpVdf = 3,
    CcIpVdf = 4,
}

impl TryFrom<i32> for FieldVdf {
    type Error = anyhow::Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::CcEosVdf),
            2 => Ok(Self::IccEosVdf),
            3 => Ok(Self::CcSpVdf),
            4 => Ok(Self::CcIpVdf),
            _ => Err(anyhow::anyhow!("invalid FieldVdf value: {value}")),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct VdfInfo {
    pub challenge: Vec<u8>,
    pub number_of_iterations: u64,
    pub output: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompactionEvent {
    pub event_id: u64,
    pub ts: u64,
    pub height: u32,
    pub header_hash: Vec<u8>,
    pub field_vdf: i32,
    pub vdf_info: Option<VdfInfo>,
    pub respond_compact_vdf: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReadCompactionEventsResponse {
    pub first_event_id: u64,
    pub last_event_id: u64,
    pub events: Vec<CompactionEvent>,
}
