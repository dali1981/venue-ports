//! A signed REST client for Binance Spot's trading API.
//!
//! Signing, the server clock and the rule for what a failed call means are
//! shared with futures, in [`super::client`]. What is spot's own is here:
//! the host, the endpoints, and the shape of the order response. See
//! <https://developers.binance.com/docs/binance-spot-api-docs>.

use crate::cex::binance::client::{ApiError, BinanceClient};
use crate::cex::binance::order::{TradeLine, VenueOrder};
use crate::cex::CexTimings;
use anyhow::{Context, Result};
use reqwest::Method;
use rust_decimal::Decimal;
use serde::Deserialize;

pub(crate) const ORDER_PATH: &str = "/api/v3/order";
pub(crate) const ORDER_TEST_PATH: &str = "/api/v3/order/test";
pub(crate) const MY_TRADES_PATH: &str = "/api/v3/myTrades";

pub struct BinanceConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_secret: String,
}

impl BinanceConfig {
    /// Reads `BINANCE_API_KEY` and `BINANCE_API_SECRET` (required — no
    /// defaults for credentials, ever) and `BINANCE_BASE_URL` (optional,
    /// defaults to the Spot **Testnet** host). Getting to production means
    /// setting `BINANCE_BASE_URL` explicitly; it is never assumed.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            base_url: std::env::var("BINANCE_BASE_URL")
                .unwrap_or_else(|_| "https://testnet.binance.vision".to_string()),
            api_key: std::env::var("BINANCE_API_KEY").context("BINANCE_API_KEY is not set")?,
            api_secret: std::env::var("BINANCE_API_SECRET")
                .context("BINANCE_API_SECRET is not set")?,
        })
    }
}

/// The subset of a spot order this crate reads, as `POST /api/v3/order`
/// (requested with `newOrderRespType=FULL`, so `fills` carries the trade
/// lines of whatever filled while the call was answered) and
/// `GET /api/v3/order` (no `fills`) both report it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BinanceOrderResponse {
    #[serde(rename = "orderId")]
    pub(crate) order_id: u64,
    pub(crate) status: String,
    #[serde(rename = "executedQty")]
    pub(crate) executed_qty: Decimal,
    #[serde(default)]
    pub(crate) fills: Vec<TradeLine>,
    /// When the venue processed the order: on the placing call's answer.
    #[serde(rename = "transactTime", default)]
    pub(crate) transact_time: Option<u64>,
    /// When the order last changed: on a status query's answer, which
    /// carries no `transactTime`.
    #[serde(rename = "updateTime", default)]
    pub(crate) update_time: Option<u64>,
}

/// What `POST /api/v3/order/test` with `computeCommissionRates=true` says an
/// order would pay, for the order's own side. The order is validated and
/// never sent to the matching engine: nothing trades. See
/// <https://developers.binance.com/docs/binance-spot-api-docs/rest-api/trading-endpoints#test-new-order-trade>.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderCheck {
    /// The quantity the order was checked with, rounded to the symbol's step
    /// as `BinanceLive::execute` would send it.
    pub quantity: Decimal,
    /// The client order id the checked request carried.
    pub client_order_id: String,
    pub rates: CommissionRates,
}

/// Commission rates on an order's trades, each a fraction of the traded
/// amount (`0.001` is 10 bps), as Binance states them for that order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommissionRates {
    #[serde(rename = "standardCommissionForOrder")]
    pub standard: MakerTaker,
    /// Absent from older answers.
    #[serde(rename = "specialCommissionForOrder", default)]
    pub special: Option<MakerTaker>,
    #[serde(rename = "taxCommissionForOrder")]
    pub tax: MakerTaker,
    /// The reduction of the standard rate when commission is paid in the
    /// discount asset (BNB).
    pub discount: Option<CommissionDiscount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MakerTaker {
    pub maker: Decimal,
    pub taker: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommissionDiscount {
    #[serde(rename = "enabledForAccount")]
    pub enabled_for_account: bool,
    #[serde(rename = "enabledForSymbol")]
    pub enabled_for_symbol: bool,
    #[serde(rename = "discountAsset")]
    pub asset: String,
    /// The fraction the standard rate is reduced by (`0.25` is a quarter).
    #[serde(rename = "discount")]
    pub rate: Decimal,
}

impl VenueOrder for BinanceOrderResponse {
    fn order_id(&self) -> u64 {
        self.order_id
    }

    fn status(&self) -> &str {
        &self.status
    }
}

pub struct BinanceRest {
    client: BinanceClient,
}

impl BinanceRest {
    pub fn new(config: BinanceConfig) -> Self {
        Self::with_timings(config, CexTimings::default())
    }

    /// As [`Self::new`], with every wait set by `timings`.
    pub fn with_timings(config: BinanceConfig, timings: CexTimings) -> Self {
        Self {
            client: BinanceClient::new(
                config.base_url,
                config.api_key,
                config.api_secret,
                "/api/v3/time",
                timings,
            ),
        }
    }

    pub(crate) fn client(&self) -> &BinanceClient {
        &self.client
    }

    /// Places a market order under `client_order_id` and returns Binance's
    /// own answer to that call. `side` is `"BUY"` or `"SELL"`; `quantity` is
    /// the venue-ready amount — rounding to the symbol's step size is
    /// `BinanceLive`'s job, not this client's.
    pub(crate) async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: Decimal,
        client_order_id: &str,
    ) -> Result<BinanceOrderResponse, ApiError> {
        let params = market_order(symbol, side, quantity, client_order_id);
        self.client.signed(Method::POST, ORDER_PATH, &params).await
    }

    /// Checks the market order [`Self::place_market_order`] would place,
    /// with the same parameters, at `POST /api/v3/order/test`, and returns
    /// the commission rates Binance says its trades would pay. Nothing is
    /// sent to the matching engine, so a lost answer is a plain failure.
    pub(crate) async fn test_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: Decimal,
        client_order_id: &str,
    ) -> Result<CommissionRates, ApiError> {
        let mut params = market_order(symbol, side, quantity, client_order_id).to_vec();
        params.push(("computeCommissionRates", "true".to_string()));
        self.client
            .signed(Method::POST, ORDER_TEST_PATH, &params)
            .await
    }
}

/// A market order's parameters, as both the order and its test send them.
fn market_order(
    symbol: &str,
    side: &str,
    quantity: Decimal,
    client_order_id: &str,
) -> [(&'static str, String); 6] {
    [
        ("symbol", symbol.to_string()),
        ("side", side.to_string()),
        ("type", "MARKET".to_string()),
        ("quantity", quantity.normalize().to_string()),
        ("newClientOrderId", client_order_id.to_string()),
        ("newOrderRespType", "FULL".to_string()),
    ]
}
