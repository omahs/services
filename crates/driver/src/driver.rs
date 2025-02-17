use crate::{
    api::{execute::ExecuteError, solve::SolveError},
    auction_converter::AuctionConverting,
    commit_reveal::{CommitRevealSolverAdapter, CommitRevealSolving, SettlementSummary},
};
use anyhow::{Context, Error, Result};
use futures::StreamExt;
use gas_estimation::GasPriceEstimating;
use model::auction::AuctionWithId;
use primitive_types::H256;
use shared::current_block::{block_number, into_stream, Block, CurrentBlockStream};
use solver::{
    driver::submit_settlement,
    driver_logger::DriverLogger,
    settlement::Settlement,
    settlement_rater::{SettlementRating, SimulationDetails},
    settlement_submission::{SolutionSubmitter, SubmissionError},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Driver {
    pub solver: Arc<dyn CommitRevealSolving>,
    pub submitter: Arc<SolutionSubmitter>,
    pub auction_converter: Arc<dyn AuctionConverting>,
    pub block_stream: CurrentBlockStream,
    pub settlement_rater: Arc<dyn SettlementRating>,
    pub logger: Arc<DriverLogger>,
    pub gas_price_estimator: Arc<dyn GasPriceEstimating>,
}

impl Driver {
    /// Does some sanity checks on the auction, collects some liquidity and prepares the auction
    /// for the solver.
    pub async fn on_auction_started(
        &self,
        auction: AuctionWithId,
    ) -> Result<SettlementSummary, SolveError> {
        // TODO get deadline from autopilot auction
        let deadline = Instant::now() + Duration::from_secs(25);
        Self::solve_until_deadline(
            auction,
            self.solver.clone(),
            self.auction_converter.clone(),
            self.block_stream.clone(),
            deadline,
        )
        .await
        .map_err(SolveError::from)
    }

    /// Computes a solution with the liquidity collected from a given block.
    async fn compute_solution_for_block(
        auction: AuctionWithId,
        block: Block,
        converter: Arc<dyn AuctionConverting>,
        solver: Arc<dyn CommitRevealSolving>,
    ) -> Result<SettlementSummary> {
        let block = block_number(&block)?;
        let auction = converter.convert_auction(auction, block).await?;
        solver.commit(auction).await
    }

    /// Keeps solving the auction in a loop with the latest known liquidity until the `deadline`
    /// has been reached or the `block_stream` terminates.
    /// This function uses a `WatchStream` to get notified about new blocks which will start with
    /// yielding the current block immediately and will skip intermediate blocks if it observed
    /// multiple blocks while computing a result.
    async fn solve_until_deadline(
        auction: AuctionWithId,
        solver: Arc<dyn CommitRevealSolving>,
        converter: Arc<dyn AuctionConverting>,
        block_stream: CurrentBlockStream,
        deadline: Instant,
    ) -> Result<SettlementSummary> {
        let compute_solutions = into_stream(block_stream.clone()).then(|block| {
            Self::compute_solution_for_block(
                auction.clone(),
                block,
                converter.clone(),
                solver.clone(),
            )
        });
        let timeout = tokio::time::sleep_until(deadline.into());
        tokio::pin!(timeout, compute_solutions);

        let mut current_solution = Err(anyhow::anyhow!("reached the deadline without a result"));
        loop {
            tokio::select! {
                new_solution = compute_solutions.next() => {
                    match new_solution {
                        Some(result) => {
                            tracing::debug!(?result, "computed new result");
                            current_solution = result;
                        },
                        None => return current_solution
                    }
                },
                _ = &mut timeout => return current_solution
            }
        }
    }

    /// Validates that the `Settlement` satisfies expected fairness and correctness properties.
    async fn validate_settlement(&self, settlement: Settlement) -> Result<SimulationDetails> {
        let gas_price = self.gas_price_estimator.estimate().await?;
        let fake_solver = Arc::new(CommitRevealSolverAdapter::from(self.solver.clone()));
        let simulation_details = self
            .settlement_rater
            .simulate_settlements(vec![(fake_solver, settlement)], gas_price)
            .await?
            .pop()
            .context("simulation returned no results")?;
        match simulation_details.gas_estimate {
            Err(err) => return Err(Error::from(err)).context("simulation failed"),
            Ok(gas_estimate) => tracing::info!(?gas_estimate, "settlement simulated successfully"),
        }
        Ok(simulation_details)
    }

    /// When the solver won the competition it finalizes the `Settlement` and decides whether it
    /// still wants to execute and submit that `Settlement`.
    pub async fn on_auction_won(&self, summary: SettlementSummary) -> Result<H256, ExecuteError> {
        tracing::info!("solver won the auction");
        let settlement = match self.solver.reveal(&summary).await? {
            None => {
                tracing::info!("solver decided against executing the settlement");
                return Err(ExecuteError::ExecutionRejected);
            }
            Some(solution) => solution,
        };
        tracing::info!(?settlement, "received settlement from solver");
        let simulation_details = self.validate_settlement(settlement).await?;
        self.submit_settlement(simulation_details)
            .await
            // TODO correctly propagate specific errors to the end
            .map_err(|e| ExecuteError::from(e.into_anyhow()))
    }

    /// Tries to submit the `Settlement` on chain. Returns a transaction hash if it was successful.
    async fn submit_settlement(
        &self,
        simulation_details: SimulationDetails,
    ) -> Result<H256, SubmissionError> {
        let gas_estimate = simulation_details
            .gas_estimate
            .expect("checked simulation gas_estimate during validation");
        tracing::info!(?gas_estimate, settlement =? simulation_details.settlement, "start submitting settlement");
        submit_settlement(
            &self.submitter,
            &self.logger,
            simulation_details.solver,
            simulation_details.settlement,
            gas_estimate,
            None, // the concept of a settlement_id does not make sense here
        )
        .await
        .map(|receipt| receipt.transaction_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auction_converter::MockAuctionConverting, commit_reveal::MockCommitRevealSolving};
    use futures::FutureExt;
    use shared::current_block::Block;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio::sync::watch::channel;

    fn block(number: Option<u64>) -> Block {
        Block {
            number: number.map(|n| n.into()),
            ..Default::default()
        }
    }

    fn deadline(milliseconds_from_now: u64) -> Instant {
        Instant::now() + Duration::from_millis(milliseconds_from_now)
    }

    #[tokio::test]
    async fn no_block_number_results_in_error() {
        let (_tx, rx) = channel(block(None));
        let converter = MockAuctionConverting::new();
        let solver = MockCommitRevealSolving::new();
        let result = Driver::solve_until_deadline(
            Default::default(),
            Arc::new(solver),
            Arc::new(converter),
            rx.clone(),
            deadline(10),
        )
        .await;

        assert_eq!(result.unwrap_err().to_string(), "no block number");
    }

    #[tokio::test]
    async fn propagates_error_from_auction_conversion() {
        let (_tx, rx) = channel(block(Some(1)));
        let mut converter = MockAuctionConverting::new();
        converter
            .expect_convert_auction()
            .returning(|_, _| async { anyhow::bail!("failed to convert auction") }.boxed());
        let solver = MockCommitRevealSolving::new();
        let result = Driver::solve_until_deadline(
            Default::default(),
            Arc::new(solver),
            Arc::new(converter),
            rx.clone(),
            deadline(10),
        )
        .await;

        assert_eq!(result.unwrap_err().to_string(), "failed to convert auction");
    }

    #[tokio::test]
    async fn propagates_error_from_auction_solving() {
        let (_tx, rx) = channel(block(Some(1)));
        let mut converter = MockAuctionConverting::new();
        converter
            .expect_convert_auction()
            .returning(|_, _| async { Ok(Default::default()) }.boxed());
        let mut solver = MockCommitRevealSolving::new();
        solver
            .expect_commit()
            .returning(|_| async { Err(anyhow::anyhow!("failed to solve auction")) }.boxed());
        let result = Driver::solve_until_deadline(
            Default::default(),
            Arc::new(solver),
            Arc::new(converter),
            rx.clone(),
            deadline(10),
        )
        .await;

        assert_eq!(result.unwrap_err().to_string(), "failed to solve auction");
    }

    #[tokio::test]
    async fn follow_up_computations_use_the_latest_block() {
        let (tx, rx) = channel(block(Some(1)));
        let mut converter = MockAuctionConverting::new();
        converter
            .expect_convert_auction()
            .returning(|_, block| {
                async move {
                    Ok(solver::solver::Auction {
                        liquidity_fetch_block: block,
                        ..Default::default()
                    })
                }
                .boxed()
            })
            .times(2);

        let mut solver = MockCommitRevealSolving::new();
        solver
            .expect_commit()
            .return_once(move |auction| {
                assert_eq!(auction.liquidity_fetch_block, 1);
                async move {
                    // there is no great place to trigger the next block so let's do it here
                    tx.send(block(Some(2))).unwrap();
                    tx.send(block(Some(3))).unwrap();
                    anyhow::bail!("failed to solve auction")
                }
                .boxed()
            })
            .times(1);
        solver
            .expect_commit()
            .returning(|auction| {
                assert_eq!(auction.liquidity_fetch_block, 3);
                async { Ok(Default::default()) }.boxed()
            })
            .times(1);

        let result = Driver::solve_until_deadline(
            Default::default(),
            Arc::new(solver),
            Arc::new(converter),
            rx.clone(),
            deadline(100),
        )
        .await
        .unwrap();
        assert_eq!(result, SettlementSummary::default());
    }

    #[tokio::test]
    async fn first_computation_starts_with_the_latest_block() {
        let (tx, rx) = channel(block(Some(1)));
        tx.send(block(Some(2))).unwrap();
        let mut converter = MockAuctionConverting::new();
        converter
            .expect_convert_auction()
            .returning(|_, block| {
                async move {
                    Ok(solver::solver::Auction {
                        liquidity_fetch_block: block,
                        ..Default::default()
                    })
                }
                .boxed()
            })
            .times(1);

        let mut solver = MockCommitRevealSolving::new();
        solver
            .expect_commit()
            .return_once(move |auction| {
                assert_eq!(auction.liquidity_fetch_block, 2);
                async move { Ok(Default::default()) }.boxed()
            })
            .times(1);

        let result = Driver::solve_until_deadline(
            Default::default(),
            Arc::new(solver),
            Arc::new(converter),
            rx.clone(),
            deadline(10),
        )
        .await
        .unwrap();
        assert_eq!(result, SettlementSummary::default());
    }

    #[tokio::test]
    async fn solving_can_end_early_when_stream_terminates() {
        let start = Instant::now();
        let (tx, rx) = channel(block(Some(1)));
        let mut converter = MockAuctionConverting::new();
        converter
            .expect_convert_auction()
            .returning(|_, block| {
                async move {
                    Ok(solver::solver::Auction {
                        liquidity_fetch_block: block,
                        ..Default::default()
                    })
                }
                .boxed()
            })
            .times(1);

        let mut solver = MockCommitRevealSolving::new();
        solver
            .expect_commit()
            .return_once(move |auction| {
                assert_eq!(auction.liquidity_fetch_block, 1);
                // drop sender to terminate the block stream while computing a result
                drop(tx);
                async move { Ok(Default::default()) }.boxed()
            })
            .times(1);

        let result = Driver::solve_until_deadline(
            Default::default(),
            Arc::new(solver),
            Arc::new(converter),
            rx.clone(),
            deadline(1_000),
        )
        .await
        .unwrap();
        assert_eq!(result, SettlementSummary::default());
        assert!(start.elapsed().as_millis() < 100);
    }
}
