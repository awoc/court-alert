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

pub struct AlertLifecycle {
    messages: Arc<dyn AlertMessageRepository>,
    pruner: DailyPruner,
}

impl AlertLifecycle {
    pub fn new(messages: Arc<dyn AlertMessageRepository>) -> Self {
        Self {
            pruner: DailyPruner::new(messages.clone()),
            messages,
        }
    }

    pub fn tracker(self: &Arc<Self>, provider: &str, surface: AlertSurface) -> AlertTracker {
        AlertTracker {
            lifecycle: self.clone(),
            provider: provider.to_owned(),
            surface,
        }
    }
}

/// A provider and surface share the lifecycle's retention policy while keeping
/// their message identity and edit plans isolated from other adapters.
pub struct AlertTracker {
    lifecycle: Arc<AlertLifecycle>,
    provider: String,
    surface: AlertSurface,
}

impl AlertTracker {
    /// Called after a successful send. A tracking failure must not resend an
    /// already delivered alert.
    pub async fn record(&self, destination: Option<&str>, message_id: &str, lines: &[AlertLine]) {
        let key = AlertMessageKey::new(&self.provider, self.surface, destination, message_id);
        if let Err(error) = self.lifecycle.messages.record_message(&key, lines).await {
            warn!(provider = %self.provider, message_id, error = %format!("{error:#}"),
                  "recording an alert failed; it cannot be updated later");
        }
    }

    /// Edit before pruning can discard tracked messages. Keep pruning shared
    /// across all providers, including when planning an edit fails.
    pub async fn strike_taken<E, F>(&self, slots: &[BookableSlotId], edit: E) -> Result<()>
    where
        E: Fn(AlertMessage) -> F,
        F: Future<Output = Result<EditOutcome>>,
    {
        let result = self.edit_taken(slots, edit).await;
        self.lifecycle.pruner.run().await;
        result
    }

    async fn edit_taken<E, F>(&self, slots: &[BookableSlotId], edit: E) -> Result<()>
    where
        E: Fn(AlertMessage) -> F,
        F: Future<Output = Result<EditOutcome>>,
    {
        if slots.is_empty() {
            return Ok(());
        }
        let messages = &self.lifecycle.messages;
        let plans = messages
            .plan_strikes(&self.provider, self.surface, slots)
            .await
            .context("planning alert edits")?;
        for plan in plans {
            let key = plan.message.key.clone();
            match edit(plan.message).await {
                Ok(EditOutcome::Edited) => {
                    if let Err(error) = messages.commit_strikes(&key, &plan.newly_struck).await {
                        warn!(provider = %key.provider, message_id = %key.id, error = %format!("{error:#}"),
                              "alert edit succeeded but recording it failed");
                    }
                }
                Ok(EditOutcome::Gone) => {
                    debug!(provider = %key.provider, message_id = %key.id, "alert message no longer exists; forgetting it");
                    if let Err(error) = messages.forget_message(&key).await {
                        warn!(provider = %key.provider, message_id = %key.id, error = %format!("{error:#}"),
                              "forgetting a deleted alert failed");
                    }
                }
                Err(error) => {
                    warn!(provider = %key.provider, message_id = %key.id, error = %format!("{error:#}"),
                                    "updating an alert failed; its tracked lines stay unchanged")
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
