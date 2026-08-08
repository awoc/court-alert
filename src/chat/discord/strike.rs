use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::model::{AlertMessage, AlertSurface, BookableSlotId};
use crate::ports::AlertMessageRepository;

pub(super) const DISCORD_UNKNOWN_MESSAGE: i64 = 10008;

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
