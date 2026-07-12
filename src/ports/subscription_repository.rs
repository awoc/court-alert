use anyhow::Result;
use async_trait::async_trait;
use chrono::NaiveDate;

use crate::domain::{ProviderUserRef, Subscription, SubscriptionDraft};

#[async_trait]
pub trait SubscriptionRepository: Send + Sync {
    async fn add(&self, subscription: SubscriptionDraft) -> Result<i64>;
    async fn list_for_user(
        &self,
        user: &ProviderUserRef,
        today: NaiveDate,
    ) -> Result<Vec<Subscription>>;
    async fn list_all(&self, today: NaiveDate) -> Result<Vec<Subscription>>;
    async fn remove(&self, id: i64, user: &ProviderUserRef) -> Result<bool>;
    async fn remove_any(&self, id: i64) -> Result<bool>;
    async fn remove_expired(&self, today: NaiveDate) -> Result<u64>;
}
