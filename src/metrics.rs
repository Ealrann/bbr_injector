use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
pub struct GaugeU64(AtomicU64);

impl GaugeU64 {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn guard_inc(&self) -> GaugeGuard<'_> {
        self.inc();
        GaugeGuard { gauge: self }
    }
}

pub struct GaugeGuard<'a> {
    gauge: &'a GaugeU64,
}

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.gauge.dec();
    }
}

#[derive(Debug, Default)]
pub struct Timer {
    count: AtomicU64,
    total_ms: AtomicU64,
    max_ms: AtomicU64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TimerSnapshot {
    pub count: u64,
    pub total_ms: u64,
    pub max_ms: u64,
    pub avg_ms: f64,
}

impl Timer {
    pub fn record(&self, dur: Duration) {
        let ms = dur.as_millis().min(u128::from(u64::MAX)) as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ms.fetch_add(ms, Ordering::Relaxed);

        let mut cur = self.max_ms.load(Ordering::Relaxed);
        while ms > cur {
            match self
                .max_ms
                .compare_exchange_weak(cur, ms, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
    }

    pub fn snapshot(&self) -> TimerSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let total_ms = self.total_ms.load(Ordering::Relaxed);
        let max_ms = self.max_ms.load(Ordering::Relaxed);
        let avg_ms = if count == 0 {
            0.0
        } else {
            (total_ms as f64) / (count as f64)
        };
        TimerSnapshot {
            count,
            total_ms,
            max_ms,
            avg_ms,
        }
    }
}
