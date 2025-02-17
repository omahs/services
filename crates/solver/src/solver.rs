use crate::interactions::allowances::AllowanceManager;
use crate::liquidity::order_converter::OrderConverter;
use crate::metrics::SolverMetrics;
use crate::settlement::external_prices::ExternalPrices;
use crate::solver::balancer_sor_solver::BalancerSorSolver;
use crate::{
    liquidity::{LimitOrder, Liquidity},
    settlement::Settlement,
};
use anyhow::{anyhow, Context, Result};
use baseline_solver::BaselineSolver;
use contracts::{BalancerV2Vault, GPv2Settlement};
use ethcontract::errors::ExecutionError;
use ethcontract::{Account, PrivateKey, H160, U256};
use http_solver::{buffers::BufferRetriever, HttpSolver};
use model::auction::AuctionId;
use naive_solver::NaiveSolver;
use num::BigRational;
use oneinch_solver::OneInchSolver;
use paraswap_solver::ParaswapSolver;
use reqwest::{Client, Url};
use shared::balancer_sor_api::DefaultBalancerSorApi;
use shared::http_solver::{DefaultHttpSolverApi, SolverConfig};
use shared::zeroex_api::ZeroExApi;
use shared::{
    baseline_solver::BaseTokens, conversions::U256Ext, token_info::TokenInfoFetching, Web3,
};
use single_order_solver::{SingleOrderSolver, SingleOrderSolving};
use std::{
    fmt::{self, Debug, Formatter},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use web3::types::AccessList;
use zeroex_solver::ZeroExSolver;

pub mod balancer_sor_solver;
mod baseline_solver;
pub mod http_solver;
mod naive_solver;
mod oneinch_solver;
mod paraswap_solver;
mod single_order_solver;
pub mod uni_v3_router_solver;
mod zeroex_solver;

#[derive(Debug)]
pub enum SolverRunError {
    Timeout,
    Solving(anyhow::Error),
}

/// Interface that all solvers must implement.
///
/// A `solve` method transforming a collection of `Liquidity` (sources) into a list of
/// independent `Settlements`. Solvers are free to choose which types `Liquidity` they
/// would like to process, including their own private sources.
#[mockall::automock]
#[async_trait::async_trait]
pub trait Solver: Send + Sync + 'static {
    /// Runs the solver.
    ///
    /// The returned settlements should be independent (for example not reusing the same user
    /// order) so that they can be merged by the driver at its leisure.
    ///
    /// id identifies this instance of solving by the driver in which it invokes all solvers.
    async fn solve(&self, auction: Auction) -> Result<Vec<Settlement>>;

    /// Returns solver's account that should be used to submit settlements.
    fn account(&self) -> &Account;

    /// Returns displayable name of the solver.
    ///
    /// This method is used for logging and metrics collection.
    fn name(&self) -> &str;
}

/// A batch auction for a solver to produce a settlement for.
#[derive(Clone, Debug)]
pub struct Auction {
    /// Note that multiple consecutive driver runs may use the same ID if the
    /// previous run was unable to find a settlement.
    pub id: AuctionId,

    /// An ID that identifies a driver run.
    ///
    /// Note that this ID is not unique across multiple instances of drivers,
    /// in particular it cannot be used to uniquely identify batches across
    /// service restarts.
    pub run: u64,

    /// The GPv2 orders to match.
    pub orders: Vec<LimitOrder>,

    /// The baseline on-chain liquidity that can be used by the solvers for
    /// settling orders.
    pub liquidity: Vec<Liquidity>,

    /// On which block the liquidity got fetched.
    pub liquidity_fetch_block: u64,

    /// The current gas price estimate.
    pub gas_price: f64,

    /// The deadline for computing a solution.
    ///
    /// This can be used internally for the solver to decide when to stop
    /// trying to optimize the settlement. The caller is expected poll the solve
    /// future at most until the deadline is reach, at which point the future
    /// will be dropped.
    pub deadline: Instant,

    /// The set of external prices for this auction.
    ///
    /// The objective value is calculated with these prices so they can be
    /// relevant for solvers.
    ///
    /// External prices are garanteed to exist for all orders included in the
    /// current auction.
    pub external_prices: ExternalPrices,
}

impl Default for Auction {
    fn default() -> Self {
        const SECONDS_IN_A_YEAR: u64 = 31_622_400;

        // Not actually never, but good enough...
        let never = Instant::now() + Duration::from_secs(SECONDS_IN_A_YEAR);
        Self {
            id: Default::default(),
            run: Default::default(),
            orders: Default::default(),
            liquidity: Default::default(),
            liquidity_fetch_block: Default::default(),
            gas_price: Default::default(),
            deadline: never,
            external_prices: Default::default(),
        }
    }
}

/// A vector of solvers.
pub type Solvers = Vec<Arc<dyn Solver>>;

/// A single settlement and a solver that produced it.
pub type SettlementWithSolver = (Arc<dyn Solver>, Settlement, Option<AccessList>);

pub type SettlementWithError = (
    Arc<dyn Solver>,
    Settlement,
    Option<AccessList>,
    ExecutionError,
);

#[derive(Copy, Clone, Debug, clap::ArgEnum)]
#[clap(rename_all = "verbatim")]
pub enum SolverType {
    Naive,
    Baseline,
    Mip,
    CowDexAg,
    OneInch,
    Paraswap,
    ZeroEx,
    Quasimodo,
    BalancerSor,
}

#[derive(Clone)]
pub enum SolverAccountArg {
    PrivateKey(PrivateKey),
    Address(H160),
}

impl Debug for SolverAccountArg {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            SolverAccountArg::PrivateKey(k) => write!(f, "PrivateKey({:?})", k.public_address()),
            SolverAccountArg::Address(a) => write!(f, "Address({a:?})"),
        }
    }
}

impl SolverAccountArg {
    pub fn into_account(self, chain_id: u64) -> Account {
        match self {
            SolverAccountArg::PrivateKey(key) => Account::Offline(key, Some(chain_id)),
            SolverAccountArg::Address(address) => Account::Local(address, None),
        }
    }
}

impl FromStr for SolverAccountArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<PrivateKey>()
            .map(SolverAccountArg::PrivateKey)
            .or_else(|pk_err| {
                Ok(SolverAccountArg::Address(s.parse().map_err(
                    |addr_err| {
                        anyhow!("could not parse as private key: {}", pk_err)
                            .context(anyhow!("could not parse as address: {}", addr_err))
                            .context("invalid solver account, it is neither a private key or an Ethereum address")
                    },
                )?))
            })
    }
}

#[derive(Debug)]
pub struct ExternalSolverArg {
    pub name: String,
    pub url: Url,
    pub account: SolverAccountArg,
}

impl FromStr for ExternalSolverArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('|');
        let name = parts.next().ok_or_else(|| anyhow!("missing name"))?;
        let url = parts.next().ok_or_else(|| anyhow!("missing url"))?;
        let account = parts.next().ok_or_else(|| anyhow!("missing account"))?;
        Ok(Self {
            name: name.to_string(),
            url: url.parse().context("parse url")?,
            account: account.parse().context("parse account")?,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub fn create(
    web3: Web3,
    solvers: Vec<(Account, SolverType)>,
    base_tokens: Arc<BaseTokens>,
    native_token: H160,
    mip_solver_url: Url,
    cow_dex_ag_solver_url: Url,
    quasimodo_solver_url: Url,
    balancer_sor_url: Url,
    settlement_contract: &GPv2Settlement,
    vault_contract: Option<&BalancerV2Vault>,
    token_info_fetcher: Arc<dyn TokenInfoFetching>,
    network_id: String,
    chain_id: u64,
    disabled_one_inch_protocols: Vec<String>,
    paraswap_slippage_bps: u32,
    disabled_paraswap_dexs: Vec<String>,
    paraswap_partner: Option<String>,
    client: Client,
    solver_metrics: Arc<dyn SolverMetrics>,
    zeroex_api: Arc<dyn ZeroExApi>,
    zeroex_slippage_bps: u32,
    disabled_zeroex_sources: Vec<String>,
    oneinch_slippage_bps: u32,
    quasimodo_uses_internal_buffers: bool,
    mip_uses_internal_buffers: bool,
    one_inch_url: Url,
    one_inch_referrer_address: Option<H160>,
    external_solvers: Vec<ExternalSolverArg>,
    oneinch_max_slippage_in_wei: Option<U256>,
    order_converter: Arc<OrderConverter>,
    max_settlements_per_solver: usize,
    max_merged_settlements: usize,
) -> Result<Solvers> {
    // Tiny helper function to help out with type inference. Otherwise, all
    // `Box::new(...)` expressions would have to be cast `as Box<dyn Solver>`.
    fn shared(solver: impl Solver + 'static) -> Arc<dyn Solver> {
        Arc::new(solver)
    }

    let buffer_retriever = Arc::new(BufferRetriever::new(
        web3.clone(),
        settlement_contract.address(),
    ));
    let allowance_mananger = Arc::new(AllowanceManager::new(
        web3.clone(),
        settlement_contract.address(),
    ));

    // We use two separate solver caches: one for our internal optimization
    // solvers (which **does** filter out orders with non-fee-connected-tokens),
    // and one for external solvers (which **does not** filter out orders with
    // non-fee-connected-tokens)
    let http_instance_with_filtered_orders = http_solver::InstanceCache::default();
    let http_instance_with_all_orders = http_solver::InstanceCache::default();

    // Helper function to create http solver instances.
    let create_http_solver = |account: Account,
                              url: Url,
                              name: String,
                              config: SolverConfig,
                              filter_non_fee_connected_orders: bool|
     -> HttpSolver {
        HttpSolver::new(
            DefaultHttpSolverApi {
                name,
                network_name: network_id.clone(),
                chain_id,
                base: url,
                client: client.clone(),
                config,
            },
            account,
            native_token,
            token_info_fetcher.clone(),
            buffer_retriever.clone(),
            allowance_mananger.clone(),
            order_converter.clone(),
            if filter_non_fee_connected_orders {
                http_instance_with_filtered_orders.clone()
            } else {
                http_instance_with_all_orders.clone()
            },
            filter_non_fee_connected_orders,
        )
    };

    let mut solvers: Vec<Arc<dyn Solver>> = solvers
        .into_iter()
        .map(|(account, solver_type)| {
            let single_order = |inner: Box<dyn SingleOrderSolving>| {
                SingleOrderSolver::new(
                    inner,
                    solver_metrics.clone(),
                    max_merged_settlements,
                    max_settlements_per_solver,
                )
            };
            let solver = match solver_type {
                SolverType::Naive => Ok(shared(NaiveSolver::new(account))),
                SolverType::Baseline => {
                    Ok(shared(BaselineSolver::new(account, base_tokens.clone())))
                }
                SolverType::Mip => Ok(shared(create_http_solver(
                    account,
                    mip_solver_url.clone(),
                    "Mip".to_string(),
                    SolverConfig {
                        use_internal_buffers: Some(mip_uses_internal_buffers),
                        ..Default::default()
                    },
                    true,
                ))),
                SolverType::CowDexAg => Ok(shared(create_http_solver(
                    account,
                    cow_dex_ag_solver_url.clone(),
                    "CowDexAg".to_string(),
                    SolverConfig::default(),
                    false,
                ))),
                SolverType::Quasimodo => Ok(shared(create_http_solver(
                    account,
                    quasimodo_solver_url.clone(),
                    "Quasimodo".to_string(),
                    SolverConfig {
                        use_internal_buffers: Some(quasimodo_uses_internal_buffers),
                        ..Default::default()
                    },
                    true,
                ))),
                SolverType::OneInch => Ok(shared(single_order(Box::new(
                    OneInchSolver::with_disabled_protocols(
                        account,
                        web3.clone(),
                        settlement_contract.clone(),
                        chain_id,
                        disabled_one_inch_protocols.clone(),
                        client.clone(),
                        one_inch_url.clone(),
                        oneinch_slippage_bps,
                        oneinch_max_slippage_in_wei,
                        one_inch_referrer_address,
                    )?,
                )))),
                SolverType::ZeroEx => {
                    let zeroex_solver = ZeroExSolver::new(
                        account,
                        web3.clone(),
                        settlement_contract.clone(),
                        chain_id,
                        zeroex_api.clone(),
                        zeroex_slippage_bps,
                        disabled_zeroex_sources.clone(),
                    )
                    .unwrap();
                    Ok(shared(single_order(Box::new(zeroex_solver))))
                }
                SolverType::Paraswap => Ok(shared(single_order(Box::new(ParaswapSolver::new(
                    account,
                    web3.clone(),
                    settlement_contract.clone(),
                    token_info_fetcher.clone(),
                    paraswap_slippage_bps,
                    disabled_paraswap_dexs.clone(),
                    client.clone(),
                    paraswap_partner.clone(),
                    None,
                ))))),
                SolverType::BalancerSor => {
                    Ok(shared(single_order(Box::new(BalancerSorSolver::new(
                        account,
                        vault_contract
                            .ok_or_else(|| {
                                anyhow!("missing Balancer Vault deployment for SOR solver")
                            })?
                            .clone(),
                        settlement_contract.clone(),
                        Arc::new(DefaultBalancerSorApi::new(
                            client.clone(),
                            balancer_sor_url.clone(),
                            chain_id,
                        )?),
                        allowance_mananger.clone(),
                    )))))
                }
            };

            if let Ok(solver) = &solver {
                tracing::info!(
                    "initialized solver {} at address {:#x}",
                    solver.name(),
                    solver.account().address()
                )
            }
            solver
        })
        .collect::<Result<_>>()?;

    let external_solvers = external_solvers.into_iter().map(|solver| {
        shared(create_http_solver(
            solver.account.into_account(chain_id),
            solver.url,
            solver.name,
            SolverConfig {
                use_internal_buffers: Some(mip_uses_internal_buffers),
                ..Default::default()
            },
            false,
        ))
    });
    solvers.extend(external_solvers);

    Ok(solvers)
}

/// Returns a naive solver to be used e.g. in e2e tests.
pub fn naive_solver(account: Account) -> Arc<dyn Solver> {
    Arc::new(NaiveSolver::new(account))
}

/// A solver that remove limit order below a certain threshold and
/// passes the remaining liquidity onto an inner solver implementation.
pub struct SellVolumeFilteringSolver {
    inner: Box<dyn Solver + Send + Sync>,
    min_value: BigRational,
}

impl SellVolumeFilteringSolver {
    pub fn new(inner: Box<dyn Solver + Send + Sync>, min_value: U256) -> Self {
        Self {
            inner,
            min_value: min_value.to_big_rational(),
        }
    }

    // The price estimates come from the Auction struct passed to solvers.
    async fn filter_orders(
        &self,
        mut orders: Vec<LimitOrder>,
        external_prices: &ExternalPrices,
    ) -> Vec<LimitOrder> {
        let is_minimum_volume = |token: &H160, amount: &U256| {
            let native_amount = external_prices.get_native_amount(*token, amount.to_big_rational());
            native_amount >= self.min_value
        };
        orders.retain(|order| {
            is_minimum_volume(&order.buy_token, &order.buy_amount)
                || is_minimum_volume(&order.sell_token, &order.sell_amount)
        });
        orders
    }
}

#[async_trait::async_trait]
impl Solver for SellVolumeFilteringSolver {
    async fn solve(&self, mut auction: Auction) -> Result<Vec<Settlement>> {
        let original_length = auction.orders.len();
        auction.orders = self
            .filter_orders(auction.orders, &auction.external_prices)
            .await;
        tracing::debug!(
            "Filtered {} orders because on insufficient volume",
            original_length - auction.orders.len()
        );
        self.inner.solve(auction).await
    }

    fn account(&self) -> &Account {
        self.inner.account()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

#[cfg(test)]
struct DummySolver;
#[cfg(test)]
#[async_trait::async_trait]
impl Solver for DummySolver {
    async fn solve(&self, _: Auction) -> Result<Vec<Settlement>> {
        todo!()
    }
    fn account(&self) -> &ethcontract::Account {
        todo!()
    }
    fn name(&self) -> &'static str {
        "DummySolver"
    }
}
#[cfg(test)]
pub fn dummy_arc_solver() -> Arc<dyn Solver> {
    Arc::new(DummySolver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{liquidity::LimitOrder, settlement::external_prices::externalprices};
    use model::order::OrderKind;
    use num::One as _;

    /// Dummy solver returning no settlements
    pub struct NoopSolver();
    #[async_trait::async_trait]
    impl Solver for NoopSolver {
        async fn solve(&self, _: Auction) -> Result<Vec<Settlement>> {
            Ok(Vec::new())
        }

        fn account(&self) -> &Account {
            unimplemented!()
        }

        fn name(&self) -> &'static str {
            "NoopSolver"
        }
    }

    #[tokio::test]
    async fn test_filtering_solver_removes_limit_orders_with_too_little_volume() {
        let sell_token = H160::from_low_u64_be(1);
        let buy_token = H160::from_low_u64_be(2);
        let orders = vec![
            // Orders with high enough amount
            LimitOrder {
                sell_amount: 100_000.into(),
                sell_token,
                buy_token,
                kind: OrderKind::Sell,
                ..Default::default()
            },
            LimitOrder {
                sell_amount: 500_000.into(),
                sell_token,
                buy_token,
                kind: OrderKind::Sell,
                ..Default::default()
            },
            // Order with small amount
            LimitOrder {
                sell_amount: 100.into(),
                sell_token,
                buy_token,
                kind: OrderKind::Sell,
                ..Default::default()
            },
        ];

        let solver = SellVolumeFilteringSolver::new(Box::new(NoopSolver()), 50_000.into());
        let prices = externalprices! { native_token: sell_token, buy_token => BigRational::one() };
        assert_eq!(solver.filter_orders(orders, &prices).await.len(), 2);
    }

    #[tokio::test]
    #[should_panic]
    async fn test_filtering_solver_panics_orders_without_price_estimate() {
        let sell_token = H160::from_low_u64_be(1);
        let orders = vec![LimitOrder {
            sell_amount: 100_000.into(),
            sell_token,
            ..Default::default()
        }];

        let prices = Default::default();
        let solver = SellVolumeFilteringSolver::new(Box::new(NoopSolver()), 0.into());
        assert_eq!(solver.filter_orders(orders, &prices).await.len(), 0);
    }

    impl PartialEq for SolverAccountArg {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (SolverAccountArg::PrivateKey(a), SolverAccountArg::PrivateKey(b)) => {
                    a.public_address() == b.public_address()
                }
                (SolverAccountArg::Address(a), SolverAccountArg::Address(b)) => a == b,
                _ => false,
            }
        }
    }

    #[test]
    fn parses_solver_account_arg() {
        assert_eq!(
            "0x4242424242424242424242424242424242424242424242424242424242424242"
                .parse::<SolverAccountArg>()
                .unwrap(),
            SolverAccountArg::PrivateKey(PrivateKey::from_raw([0x42; 32]).unwrap())
        );
        assert_eq!(
            "0x4242424242424242424242424242424242424242"
                .parse::<SolverAccountArg>()
                .unwrap(),
            SolverAccountArg::Address(H160([0x42; 20])),
        );
    }

    #[test]
    fn errors_on_invalid_solver_account_arg() {
        assert!("0x010203040506070809101112131415161718192021"
            .parse::<SolverAccountArg>()
            .is_err());
        assert!("not an account".parse::<SolverAccountArg>().is_err());
    }

    #[test]
    fn parse_external_solver_arg() {
        let arg = "name|http://solver.com/|0x4242424242424242424242424242424242424242424242424242424242424242";
        let parsed = ExternalSolverArg::from_str(arg).unwrap();
        assert_eq!(parsed.name, "name");
        assert_eq!(parsed.url.to_string(), "http://solver.com/");
        assert_eq!(
            parsed.account,
            SolverAccountArg::PrivateKey(PrivateKey::from_raw([0x42; 32]).unwrap())
        );
    }
}
