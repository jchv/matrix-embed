use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use matrix_sdk::ruma::OwnedEventId;
use tokio::sync::Mutex;
use tracing::debug;
use url::Url;

/// Maximum age of tracked events before they are eligible for cleanup.
const MAX_EVENT_AGE: Duration = Duration::from_secs(15 * 60);

/// How often the background cleanup task runs.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct TrackedEntry {
    /// The URL that was extracted from the message (if any).
    pub extracted_url: Option<Url>,
    /// Reply event ID (if any).
    pub reply_event_id: Option<OwnedEventId>,
    /// When this entry was created.
    created_at: Instant,
}

/// Tracks embed tasks keyed by the original message's event ID.
pub struct EventTracker {
    entries: Mutex<HashMap<OwnedEventId, TrackedEntry>>,
}

impl EventTracker {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Register a new embed task for `original_event_id`.
    pub async fn register(
        &self,
        original_event_id: OwnedEventId,
        url: Option<Url>,
        reply_event_id: Option<OwnedEventId>,
    ) {
        let mut entries = self.entries.lock().await;

        entries.insert(
            original_event_id,
            TrackedEntry {
                extracted_url: url,
                reply_event_id,
                created_at: Instant::now(),
            },
        );
    }

    pub async fn get_event_entry(&self, original_event_id: &OwnedEventId) -> Option<TrackedEntry> {
        let entries = self.entries.lock().await;
        entries.get(original_event_id).cloned()
    }

    /// Remove entries older than [`MAX_EVENT_AGE`].
    pub async fn cleanup(&self) {
        let mut entries = self.entries.lock().await;
        let before = entries.len();
        entries.retain(|_, entry| entry.created_at.elapsed() < MAX_EVENT_AGE);
        let removed = before - entries.len();
        if removed > 0 {
            debug!(
                "Tracker cleanup: removed {} stale entries ({} remaining)",
                removed,
                entries.len()
            );
        }
    }

    /// Spawn a background tokio task that calls [`cleanup`](Self::cleanup)
    /// at regular intervals.
    pub fn spawn_cleanup_task(self: &Arc<Self>) {
        let tracker = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CLEANUP_INTERVAL).await;
                tracker.cleanup().await;
            }
        });
    }
}
