//! Injectable clock abstraction for deterministic time handling.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Source of UTC Unix timestamps in seconds.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now_unix(&self) -> u64;
}

/// Production wall clock backed by [`SystemTime`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

/// Deterministic clock for tests and simulations.
#[derive(Debug)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub fn new(initial_unix: u64) -> Self {
        Self {
            now: AtomicU64::new(initial_unix),
        }
    }

    pub fn set(&self, unix: u64) {
        self.now.store(unix, Ordering::SeqCst);
    }

    pub fn advance(&self, seconds: u64) -> u64 {
        self.now
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_add(seconds))
            })
            .unwrap_or_else(|v| v)
            .saturating_add(seconds)
    }
}

impl Clock for ManualClock {
    fn now_unix(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}
