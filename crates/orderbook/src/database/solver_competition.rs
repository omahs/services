use super::Postgres;
use crate::solver_competition::{Identifier, LoadSolverCompetitionError, SolverCompetitionStoring};
use anyhow::{Context, Result};
use database::byte_array::ByteArray;
use model::solver_competition::SolverCompetition;

#[async_trait::async_trait]
impl SolverCompetitionStoring for Postgres {
    async fn save(&self, data: SolverCompetition) -> Result<()> {
        let _timer = super::Metrics::get()
            .database_queries
            .with_label_values(&["save_solver_competition"])
            .start_timer();

        let tx_hash = data.transaction_hash.map(|h256| ByteArray(h256.0));
        let mut ex = self.pool.acquire().await?;
        database::solver_competition::save(
            &mut ex,
            data.auction_id,
            &serde_json::to_value(&data)?,
            tx_hash.as_ref(),
        )
        .await
        .context("failed to insert solver competition")?;
        Ok(())
    }

    async fn load(&self, id: Identifier) -> Result<SolverCompetition, LoadSolverCompetitionError> {
        let _timer = super::Metrics::get()
            .database_queries
            .with_label_values(&["load_solver_competition"])
            .start_timer();

        let mut ex = self.pool.acquire().await.map_err(anyhow::Error::from)?;
        let value = match id {
            Identifier::Id(id) => database::solver_competition::load_by_id(&mut ex, id).await,
            Identifier::Transaction(hash) => {
                database::solver_competition::load_by_tx_hash(&mut ex, &ByteArray(hash.0)).await
            }
        }
        .context("failed to get solver competition by ID")?;
        match value {
            None => Err(LoadSolverCompetitionError::NotFound),
            Some(value) => serde_json::from_value(value)
                .map_err(anyhow::Error::from)
                .map_err(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::solver_competition::{CompetitionAuction, SolverSettlement};
    use primitive_types::H256;

    #[tokio::test]
    #[ignore]
    async fn postgres_solver_competition_roundtrip() {
        let db = Postgres::new("postgresql://").unwrap();
        database::clear_DANGER(&db.pool).await.unwrap();

        let expected = SolverCompetition {
            auction_id: 0,
            gas_price: 1.,
            auction_start_block: 2,
            liquidity_collected_block: 3,
            competition_simulation_block: 4,
            transaction_hash: Some(H256([5; 32])),
            auction: CompetitionAuction {
                orders: vec![Default::default()],
                prices: [Default::default()].into_iter().collect(),
            },
            solutions: vec![SolverSettlement {
                solver: "asdf".to_string(),
                objective: Default::default(),
                clearing_prices: [Default::default()].into_iter().collect(),
                orders: vec![Default::default()],
                call_data: vec![1, 2],
            }],
        };
        db.save(expected.clone()).await.unwrap();
        let actual = db.load(Identifier::Id(0)).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    #[ignore]
    async fn not_found_error() {
        let db = Postgres::new("postgresql://").unwrap();
        database::clear_DANGER(&db.pool).await.unwrap();

        let result = db
            .load(Identifier::Transaction(Default::default()))
            .await
            .unwrap_err();
        assert!(matches!(result, LoadSolverCompetitionError::NotFound));
    }
}
