use anyhow::Result;
use async_trait::async_trait;

use crate::model::{BookableSlot, BookableSlotSnapshot};

#[async_trait]
pub trait BookableSlotSnapshotRepository: Send + Sync {
    async fn load_snapshot(&self) -> Result<BookableSlotSnapshot>;
    async fn replace_snapshot(&self, slots: Vec<BookableSlot>) -> Result<()>;
}
