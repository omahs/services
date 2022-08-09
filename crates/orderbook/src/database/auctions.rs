use anyhow::Result;
use model::auction::Auction;

#[async_trait::async_trait]
pub trait AuctionRetrieval: Send + Sync {
    async fn most_recent_auction(&self) -> Result<Option<Auction>>;
}

#[async_trait::async_trait]
impl AuctionRetrieval for super::Postgres {
    async fn most_recent_auction(&self) -> Result<Option<Auction>> {
        let _timer = super::Metrics::get()
            .database_queries
            .with_label_values(&["load_most_recent_auction"])
            .start_timer();

        let mut ex = self.pool.acquire().await?;
        let (auction_id, json) = match database::auction::load_most_recent(&mut ex).await? {
            Some(inner) => inner,
            None => return Ok(None),
        };
        // TODO: what about auction_id? Add to Auction? Make it replace competition id?
        let auction: Auction = serde_json::from_value(json)?;
        Ok(Some(auction))
    }
}
