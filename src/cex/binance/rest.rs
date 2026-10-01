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
        let params = [
            ("symbol", symbol.to_string()),
            ("side", side.to_string()),
            ("type", "MARKET".to_string()),
            ("quantity", quantity.normalize().to_string()),
            ("newClientOrderId", client_order_id.to_string()),
            ("newOrderRespType", "FULL".to_string()),
        ];
        self.client.signed(Method::POST, ORDER_PATH, &params).await
    }
}
