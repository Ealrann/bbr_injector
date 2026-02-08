use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct Shutdown {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    triggered: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                triggered: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    pub fn trigger(&self) {
        if self.inner.triggered.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.notify.notify_waiters();
    }

    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::Relaxed)
    }

    pub async fn wait(&self) {
        if self.is_triggered() {
            return;
        }
        self.inner.notify.notified().await;
    }
}
