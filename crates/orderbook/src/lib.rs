pub mod api;
pub mod arguments;
pub mod database;
pub mod orderbook;
pub mod solver_competition;

use crate::database::trades::TradeRetrieving;
use crate::orderbook::Orderbook;
use anyhow::{anyhow, Context as _, Result};
use contracts::GPv2Settlement;
use futures::Future;
use model::DomainSeparator;
use shared::order_quoting::QuoteHandler;
use solver_competition::SolverCompetitionStoring;
use std::{net::SocketAddr, sync::Arc};
use tokio::{task, task::JoinHandle};
use warp::Filter;

#[allow(clippy::too_many_arguments)]
pub fn serve_api(
    database: Arc<dyn TradeRetrieving>,
    orderbook: Arc<Orderbook>,
    quotes: Arc<QuoteHandler>,
    address: SocketAddr,
    shutdown_receiver: impl Future<Output = ()> + Send + 'static,
    solver_competition: Arc<dyn SolverCompetitionStoring>,
    solver_competition_auth: Option<String>,
) -> JoinHandle<()> {
    let filter = api::handle_all_routes(
        database,
        orderbook,
        quotes,
        solver_competition,
        solver_competition_auth,
    )
    .boxed();
    tracing::info!(%address, "serving order book");
    let (_, server) = warp::serve(filter).bind_with_graceful_shutdown(address, shutdown_receiver);
    task::spawn(server)
}

/**
 * Check that important constants such as the EIP 712 Domain Separator and Order Type Hash used in this binary match the ones on the deployed contract instance.
 * Signature inconsistencies due to a mismatch of these constants are hard to debug.
 */
pub async fn verify_deployed_contract_constants(
    contract: &GPv2Settlement,
    chain_id: u64,
) -> Result<()> {
    let web3 = contract.raw_instance().web3();
    let bytecode = hex::encode(
        web3.eth()
            .code(contract.address(), None)
            .await
            .context("Could not load deployed bytecode")?
            .0,
    );

    let domain_separator = DomainSeparator::new(chain_id, contract.address());
    if !bytecode.contains(&hex::encode(domain_separator.0)) {
        return Err(anyhow!("Bytecode did not contain domain separator"));
    }

    if !bytecode.contains(&hex::encode(model::order::OrderData::TYPE_HASH)) {
        return Err(anyhow!("Bytecode did not contain order type hash"));
    }
    Ok(())
}
