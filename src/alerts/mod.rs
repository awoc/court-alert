//! Shared recording, edit persistence, and retention for alert messages.

mod prune;

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::model::{AlertLine, AlertMessage, AlertMessageKey, AlertSurface, BookableSlotId};
use crate::ports::AlertMessageRepository;
use prune::DailyPruner;

pub enum EditOutcome {
    Edited,
    Gone,
}

pub struct AlertMessageLifecycle {
    messages: Arc<dyn AlertMessageRepository>,
    pruner: DailyPruner,
}

impl AlertMessageLifecycle {
    pub fn new(messages: Arc<dyn AlertMessageRepository>) -> Self {
        Self {
            pruner: DailyPruner::new(messages.clone()),
            messages,
        }
    }

    pub fn tracker(
        self: &Arc<Self>,
        chat_provider: &str,
        surface: AlertSurface,
    ) -> AlertMessageTracker {
        AlertMessageTracker {
            lifecycle: self.clone(),
            chat_provider: chat_provider.to_owned(),
            surface,
        }
    }
}

pub struct AlertMessageTracker {
    lifecycle: Arc<AlertMessageLifecycle>,
    chat_provider: String,
    surface: AlertSurface,
}

impl AlertMessageTracker {
    /// Records a successfully delivered alert. Persistence errors are logged.
    pub async fn record(&self, destination: Option<&str>, message_id: &str, lines: &[AlertLine]) {
        let key = AlertMessageKey::new(&self.chat_provider, self.surface, destination, message_id);
        if let Err(error) = self.lifecycle.messages.record_message(&key, lines).await {
            warn!(
                chat_provider = %self.chat_provider,
                message_id,
                error = %format!("{error:#}"),
                "recording an alert failed; it cannot be updated later"
            );
        }
    }

    /// Edits messages for taken slots, then runs shared retention.
    /// Returns planning errors; individual edit and persistence errors are logged.
    pub async fn mark_taken<E, F>(&self, slots: &[BookableSlotId], edit: E) -> Result<()>
    where
        E: Fn(AlertMessage) -> F,
        F: Future<Output = Result<EditOutcome>>,
    {
        let result = self.apply_edits(slots, edit).await;
        self.lifecycle.pruner.run().await;
        result
    }

    async fn apply_edits<E, F>(&self, slots: &[BookableSlotId], edit: E) -> Result<()>
    where
        E: Fn(AlertMessage) -> F,
        F: Future<Output = Result<EditOutcome>>,
    {
        if slots.is_empty() {
            return Ok(());
        }
        let messages = &self.lifecycle.messages;
        let plans = messages
            .plan_strikes(&self.chat_provider, self.surface, slots)
            .await
            .context("planning alert edits")?;
        for plan in plans {
            let key = plan.message.key.clone();
            match edit(plan.message).await {
                Ok(EditOutcome::Edited) => {
                    if let Err(error) = messages.commit_strikes(&key, &plan.newly_struck).await {
                        warn!(
                            chat_provider = %key.chat_provider,
                            message_id = %key.id,
                            error = %format!("{error:#}"),
                            "alert edit succeeded but recording it failed"
                        );
                    }
                }
                Ok(EditOutcome::Gone) => {
                    debug!(
                        chat_provider = %key.chat_provider,
                        message_id = %key.id,
                        "alert message no longer exists; forgetting it"
                    );
                    if let Err(error) = messages.forget_message(&key).await {
                        warn!(
                            chat_provider = %key.chat_provider,
                            message_id = %key.id,
                            error = %format!("{error:#}"),
                            "forgetting a deleted alert failed"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        chat_provider = %key.chat_provider,
                        message_id = %key.id,
                        error = %format!("{error:#}"),
                        "updating an alert failed; its tracked lines stay unchanged"
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn skip_pruning_today(&self) {
        self.lifecycle.pruner.skip_today();
    }

    #[cfg(test)]
    pub(crate) fn last_pruned(&self) -> Option<chrono::NaiveDate> {
        self.lifecycle.pruner.last_run()
    }
}

#[cfg(test)]
mod tests;
