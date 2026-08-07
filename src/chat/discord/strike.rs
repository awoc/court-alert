use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use tracing::{debug, warn};

use crate::model::{AlertMessage, AlertSurface, BookableSlotId};
use crate::ports::AlertMessageRepository;
use crate::time::today_berlin;

const PRUNE_GRACE: chrono::TimeDelta = chrono::TimeDelta::hours(1);

pub(super) enum EditOutcome {
    Edited,
    Gone,
}

pub(super) async fn strike_through<E, F>(
    messages: &Arc<dyn AlertMessageRepository>,
    surface: AlertSurface,
    slots: &[BookableSlotId],
    edit: E,
) -> Result<()>
where
    E: Fn(AlertMessage) -> F,
    F: Future<Output = Result<EditOutcome>>,
{
    if slots.is_empty() {
        return Ok(());
    }
    let plans = messages
        .plan_strikes(surface, slots)
        .await
        .context("planning strikethroughs")?;
    if plans.is_empty() {
        debug!(
            slots = slots.len(),
            "discord: no tracked message holds these slots; nothing to strike"
        );
        return Ok(());
    }

    for plan in plans {
        let message_id = plan.message.id.clone();
        match edit(plan.message).await {
            Ok(EditOutcome::Edited) => {
                if let Err(error) = messages
                    .commit_strikes(&message_id, &plan.newly_struck)
                    .await
                {
                    warn!(
                        error = %format!("{error:#}"),
                        message_id,
                        "discord: edit succeeded but recording it failed"
                    );
                }
            }
            Ok(EditOutcome::Gone) => {
                debug!(
                    message_id,
                    "discord: message no longer exists; forgetting it"
                );
                if let Err(error) = messages.forget_message(&message_id).await {
                    warn!(
                        error = %format!("{error:#}"),
                        message_id,
                        "discord: forgetting a deleted message failed"
                    );
                }
            }
            Err(error) => warn!(
                error = %format!("{error:#}"),
                message_id,
                "discord: striking through an alert failed; it stays live-looking"
            ),
        }
    }
    Ok(())
}

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

    pub(super) async fn run(&self) {
        let today = today_berlin();
        if *self.last_pruned.lock().expect("prune guard poisoned") == Some(today) {
            return;
        }
        // Grace, so a strikethrough already in flight is not pruned out from under it.
        match self.messages.prune_ended(Utc::now() - PRUNE_GRACE).await {
            Ok(removed) => {
                *self.last_pruned.lock().expect("prune guard poisoned") = Some(today);
                if removed > 0 {
                    debug!(
                        removed,
                        "discord: pruned alert messages whose slots all ended"
                    );
                }
            }
            Err(error) => warn!(
                error = %format!("{error:#}"),
                "discord: pruning alert messages failed"
            ),
        }
    }

    #[cfg(test)]
    pub(super) fn last_run(&self) -> Option<NaiveDate> {
        *self.last_pruned.lock().unwrap()
    }

    #[cfg(test)]
    pub(super) fn skip_today(&self) {
        *self.last_pruned.lock().unwrap() = Some(today_berlin());
    }
}
