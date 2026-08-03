use anyhow::Result;
use async_trait::async_trait;

use crate::model::{CourtCatalog, Venue};

#[async_trait]
pub trait CourtCatalogSource: Send + Sync {
    async fn discover(&self, venue: &Venue) -> Result<CourtCatalog>;
}
