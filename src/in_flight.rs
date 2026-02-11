use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    Announcement,
    RespondOnly,
}

impl Default for SendMode {
    fn default() -> Self {
        Self::Announcement
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightState {
    #[serde(default)]
    pub cursor_event_id: u64,
    #[serde(default)]
    pub end_event_id: Option<u64>,
    #[serde(default)]
    pub proofs_per_window: u32,
    #[serde(default)]
    pub window_ms: u64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub send_mode: SendMode,
}

impl InFlightState {
    pub fn apply_defaults(mut self, defaults: &Self) -> Self {
        if self.proofs_per_window == 0 {
            self.proofs_per_window = defaults.proofs_per_window;
        }
        if self.window_ms == 0 {
            self.window_ms = defaults.window_ms;
        }

        self.proofs_per_window = self.proofs_per_window.max(1);
        self.window_ms = self.window_ms.max(1);

        self
    }
}

#[derive(Clone)]
pub struct InFlightStore {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    state: Mutex<InFlightState>,
}

impl InFlightStore {
    pub async fn load_or_init(path: PathBuf, defaults: InFlightState) -> anyhow::Result<Self> {
        let loaded = match tokio::fs::read(&path).await {
            Ok(data) => serde_json::from_slice::<InFlightState>(&data)
                .with_context(|| format!("parse in-flight json {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => defaults.clone(),
            Err(e) => return Err(e).with_context(|| format!("read in-flight json {}", path.display())),
        };
        let loaded = loaded.apply_defaults(&defaults);

        let store = Self {
            inner: Arc::new(Inner {
                path,
                state: Mutex::new(loaded),
            }),
        };
        store.persist().await?;
        Ok(store)
    }

    pub async fn snapshot(&self) -> InFlightState {
        self.inner.state.lock().await.clone()
    }

    pub async fn set_cursor_event_id(&self, cursor_event_id: u64) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        state.cursor_event_id = cursor_event_id;
        self.persist_locked(&state).await
    }

    pub async fn set_speed(&self, proofs_per_window: u32, window_ms: u64) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        state.proofs_per_window = proofs_per_window.max(1);
        state.window_ms = window_ms.max(1);
        self.persist_locked(&state).await
    }

    pub async fn set_cursor_and_end(
        &self,
        cursor_event_id: u64,
        end_event_id: Option<u64>,
    ) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        state.cursor_event_id = cursor_event_id;
        state.end_event_id = end_event_id;
        self.persist_locked(&state).await
    }

    pub async fn set_paused(&self, paused: bool) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        state.paused = paused;
        self.persist_locked(&state).await
    }

    pub async fn set_send_mode(&self, send_mode: SendMode) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        state.send_mode = send_mode;
        self.persist_locked(&state).await
    }

    async fn persist(&self) -> anyhow::Result<()> {
        let state = self.inner.state.lock().await;
        self.persist_locked(&state).await
    }

    async fn persist_locked(&self, state: &InFlightState) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(state).context("serialize in-flight json")?;
        write_atomic_json(&self.inner.path, &bytes).await
    }
}

async fn write_atomic_json(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("create in-flight dir {}", dir.display()))?;
    }

    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("write in-flight tmp {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("rename in-flight tmp -> {}", path.display()))?;

    Ok(())
}
