//! Background enforcement of the size and age budget.

use crate::error::CacheError;
use crate::http::CacheState;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Evicts per the retention policy and deletes the evicted blobs; returns the eviction count.
pub(crate) async fn sweep(state: &CacheState) -> Result<usize, CacheError> {
    let now = state.clock.now_unix();
    let swept = state.index.sweep(now, state.config.retention).await?;
    if let Ok(mut active) = state.active_reservations.write() {
        for blob in &swept.blobs {
            active.remove(blob);
        }
        active.retain(|_, stale_after| *stale_after > now);
    }
    for blob in &swept.blobs {
        if let Err(err) = state.store.delete(blob).await {
            tracing::warn!(blob = %blob, error = %err, "evicted cache blob could not be deleted");
        }
    }
    Ok(swept.blobs.len())
}

/// Sweeps on every interval and whenever an upload is finalized, until cancelled.
pub(crate) async fn run(state: Arc<CacheState>, cancel: CancellationToken) {
    loop {
        if let Err(err) = sweep(&state).await {
            tracing::warn!(error = %err, "cache retention sweep failed");
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(state.config.sweep_interval) => {}
            () = state.sweep_requested.notified() => {}
        }
    }
}
