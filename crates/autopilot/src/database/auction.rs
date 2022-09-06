use super::Postgres;
use anyhow::{anyhow, Context, Result};
use database::{
    auction::AuctionId,
    orders::{
        BuyTokenDestination as DbBuyTokenDestination, OrderKind as DbOrderKind,
        SellTokenSource as DbSellTokenSource, SigningScheme as DbSigningScheme,
    },
};
use futures::{StreamExt, TryStreamExt};
use model::{
    app_id::AppId,
    auction::{Auction, Order, OrderMetadata},
    order::{BuyTokenDestination, OrderData, OrderKind, OrderUid, SellTokenSource},
    signature::{Signature, SigningScheme},
};
use number_conversions::big_decimal_to_u256;
use primitive_types::H160;

pub struct SolvableOrders {
    pub orders: Vec<Order>,
    pub latest_settlement_block: u64,
}

impl Postgres {
    pub async fn solvable_orders(&self, min_valid_to: u32) -> Result<SolvableOrders> {
        let _timer = super::Metrics::get()
            .database_queries
            .with_label_values(&["solvable_orders"])
            .start_timer();

        let mut ex = self.0.begin().await?;
        let orders = database::orders::solvable_orders(&mut ex, min_valid_to as i64)
            .map(|result| match result {
                Ok(order) => full_order_into_model_order(order),
                Err(err) => Err(anyhow::Error::from(err)),
            })
            .try_collect::<Vec<_>>()
            .await?;
        let latest_settlement_block =
            database::orders::latest_settlement_block(&mut ex).await? as u64;
        Ok(SolvableOrders {
            orders,
            latest_settlement_block,
        })
    }

    pub async fn replace_current_auction(&self, auction: &Auction) -> Result<AuctionId> {
        let _timer = super::Metrics::get()
            .database_queries
            .with_label_values(&["save_auction"])
            .start_timer();

        let data = serde_json::to_value(&auction)?;
        let mut ex = self.0.begin().await?;
        database::auction::delete_all_auctions(&mut ex).await?;
        let id = database::auction::save(&mut ex, &data).await?;
        ex.commit().await?;
        Ok(id)
    }
}

fn full_order_into_model_order(order: database::orders::FullOrder) -> Result<Order> {
    let metadata = OrderMetadata {
        creation_date: order.creation_timestamp,
        owner: H160(order.owner.0),
        uid: OrderUid(order.uid.0),
        executed_amount: big_decimal_to_u256(&match order.kind {
            DbOrderKind::Buy => order.sum_buy,
            DbOrderKind::Sell => order.sum_sell - order.sum_fee,
        })
        .context("executed_amount does not fit into u256")?,
        full_fee_amount: big_decimal_to_u256(&order.full_fee_amount)
            .ok_or_else(|| anyhow!("full_fee_amount is not U256"))?,
        is_liquidity_order: order.is_liquidity_order,
    };
    let data = OrderData {
        sell_token: H160(order.sell_token.0),
        buy_token: H160(order.buy_token.0),
        receiver: order.receiver.map(|address| H160(address.0)),
        sell_amount: big_decimal_to_u256(&order.sell_amount)
            .ok_or_else(|| anyhow!("sell_amount is not U256"))?,
        buy_amount: big_decimal_to_u256(&order.buy_amount)
            .ok_or_else(|| anyhow!("buy_amount is not U256"))?,
        valid_to: order.valid_to.try_into().context("valid_to is not u32")?,
        app_data: AppId(order.app_data.0),
        fee_amount: big_decimal_to_u256(&order.fee_amount)
            .ok_or_else(|| anyhow!("fee_amount is not U256"))?,
        kind: order_kind_from(order.kind),
        partially_fillable: order.partially_fillable,
        sell_token_balance: sell_token_source_from(order.sell_token_balance),
        buy_token_balance: buy_token_destination_from(order.buy_token_balance),
    };
    let signing_scheme = signing_scheme_from(order.signing_scheme);
    let signature = Signature::from_bytes(signing_scheme, &order.signature)?;
    Ok(Order {
        metadata,
        data,
        signature,
    })
}

pub fn order_kind_from(kind: DbOrderKind) -> OrderKind {
    match kind {
        DbOrderKind::Buy => OrderKind::Buy,
        DbOrderKind::Sell => OrderKind::Sell,
    }
}

fn sell_token_source_from(source: DbSellTokenSource) -> SellTokenSource {
    match source {
        DbSellTokenSource::Erc20 => SellTokenSource::Erc20,
        DbSellTokenSource::Internal => SellTokenSource::Internal,
        DbSellTokenSource::External => SellTokenSource::External,
    }
}

fn buy_token_destination_from(destination: DbBuyTokenDestination) -> BuyTokenDestination {
    match destination {
        DbBuyTokenDestination::Erc20 => BuyTokenDestination::Erc20,
        DbBuyTokenDestination::Internal => BuyTokenDestination::Internal,
    }
}

fn signing_scheme_from(scheme: DbSigningScheme) -> SigningScheme {
    match scheme {
        DbSigningScheme::Eip712 => SigningScheme::Eip712,
        DbSigningScheme::EthSign => SigningScheme::EthSign,
        DbSigningScheme::Eip1271 => SigningScheme::Eip1271,
        DbSigningScheme::PreSign => SigningScheme::PreSign,
    }
}
