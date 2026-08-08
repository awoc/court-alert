//! Retention of the alert messages each surface has announced slots in.

use std::sync::{Arc, Mutex};

use chrono::{NaiveDate, Utc};
use tracing::{debug, warn};

use crate::ports::AlertMessageRepository;
use crate::time::today_berlin;

const PRUNE_GRACE: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// Drops tracked alert messages whose slots have all ended.
pub struct DailyPruner {
    messages: Arc<dyn AlertMessageRepository>,
    last_pruned: Mutex<Option<NaiveDate>>,
}

impl DailyPruner {
    pub fn new(messages: Arc<dyn AlertMessageRepository>) -> Self {
        Self {
            messages,
            last_pruned: Mutex::new(None),
        }
    }

    pub async fn run(&self) {
        let today = today_berlin();
        {
            // Claimed before the await: checking, then awaiting, lets both callers past.
            let mut last_pruned = self.last_pruned.lock().expect("prune guard poisoned");
            if *last_pruned == Some(today) {
                return;
            }
            *last_pruned = Some(today);
        }
        // Grace, so a strikethrough already in flight is not pruned out from under it.
        match self.messages.prune_ended(Utc::now() - PRUNE_GRACE).await {
            Ok(removed) => {
                if removed > 0 {
                    debug!(removed, "pruned alert messages whose slots all ended");
                }
            }
            Err(error) => {
                *self.last_pruned.lock().expect("prune guard poisoned") = None;
                warn!(
                    error = %format!("{error:#}"),
                    "pruning alert messages failed"
                );
            }
        }
    }

    #[cfg(test)]
    pub fn last_run(&self) -> Option<NaiveDate> {
        *self.last_pruned.lock().unwrap()
    }

    #[cfg(test)]
    pub fn skip_today(&self) {
        *self.last_pruned.lock().unwrap() = Some(today_berlin());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AlertLine, AlertSurface, StrikePlan};
    use anyhow::Result;
    use chrono::{DateTime, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BlockingPruneRepository {
        prunes: AtomicUsize,
        started: tokio::sync::mpsc::UnboundedSender<()>,
        release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl AlertMessageRepository for BlockingPruneRepository {
        async fn record_message(
            &self,
            _surface: AlertSurface,
            _channel_id: Option<&str>,
            _message_id: &str,
            _lines: &[AlertLine],
        ) -> Result<()> {
            unimplemented!("not used by these tests")
        }

        async fn plan_strikes(
            &self,
            _surface: AlertSurface,
            _slots: &[crate::model::BookableSlotId],
        ) -> Result<Vec<StrikePlan>> {
            unimplemented!("not used by these tests")
        }

        async fn commit_strikes(&self, _message_id: &str, _lines: &[u32]) -> Result<()> {
            unimplemented!("not used by these tests")
        }

        async fn forget_message(&self, _message_id: &str) -> Result<()> {
            unimplemented!("not used by these tests")
        }

        async fn prune_ended(&self, _now: DateTime<Utc>) -> Result<usize> {
            self.prunes.fetch_add(1, Ordering::SeqCst);
            let _ = self.started.send(());
            let waiting = self.release.lock().expect("release guard poisoned").take();
            if let Some(waiting) = waiting {
                let _ = waiting.await;
            }
            Ok(0)
        }
    }

    #[tokio::test]
    async fn a_second_caller_does_not_start_a_prune_while_one_is_running() {
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let (release, held) = tokio::sync::oneshot::channel();
        let messages = Arc::new(BlockingPruneRepository {
            prunes: AtomicUsize::new(0),
            started,
            release: std::sync::Mutex::new(Some(held)),
        });
        let pruner = Arc::new(DailyPruner::new(messages.clone()));

        let first = tokio::spawn({
            let pruner = pruner.clone();
            async move { pruner.run().await }
        });
        starts.recv().await.expect("the first prune never started");

        pruner.run().await;

        assert_eq!(
            messages.prunes.load(Ordering::SeqCst),
            1,
            "the second caller started its own full-table delete"
        );
        release.send(()).unwrap();
        first.await.unwrap();
    }
}
