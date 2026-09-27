//! A signed REST client for Binance Spot's trading API.
//!
//! Every private endpoint follows the same shape: a query string carrying
//! `timestamp` (and, here, `recvWindow`), HMAC-SHA256-signed with the API
//! secret, the signature appended as a final `signature` parameter, and the
//! API key carried in the `X-MBX-APIKEY` header (never in the query string
//! itself). See <https://developers.binance.com/docs/binance-spot-api-docs>.

use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// How long, in milliseconds, a signed request stays valid after its
/// `timestamp` — Binance rejects anything older than this against its own
/// server clock. 5s is comfortably inside Binance's default 5000ms
/// `recvWindow` while leaving room for network latency.
const RECV_WINDOW_MS: u64 = 5_000;

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

/// One fill line from an order's own placement response.
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceFill {
    pub price: Decimal,
    pub qty: Decimal,
    pub commission: Decimal,
    #[serde(rename = "commissionAsset")]
    pub commission_asset: String,
}

/// The subset of Binance's `POST /api/v3/order` response this crate reads.
/// Requested with `newOrderRespType=FULL` so `fills` is always present for
/// a filled market order — the placing call's own response, never a
/// separate status query, per `SPEC.md` §6's fill-reading rule.
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceOrderResponse {
    #[serde(rename = "orderId")]
    pub order_id: u64,
    pub status: String,
    #[serde(default)]
    pub fills: Vec<BinanceFill>,
}

pub struct BinanceRest {
    config: BinanceConfig,
    http: reqwest::Client,
}

impl BinanceRest {
    pub fn new(config: BinanceConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    /// Places a market order and returns Binance's own response to that
    /// call. `side` is `"BUY"` or `"SELL"`; `quantity` is the venue-ready
    /// amount — rounding to the symbol's step size is `BinanceLive`'s job,
    /// not this client's.
    pub async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: Decimal,
    ) -> Result<BinanceOrderResponse> {
        let query = format!(
            "symbol={symbol}&side={side}&type=MARKET&quantity={quantity}&newOrderRespType=FULL"
        );
        let response = self.signed_post("/api/v3/order", &query).await?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("Binance rejected the order: {body}");
        }
        response
            .json()
            .await
            .context("decoding Binance order response")
    }

    async fn signed_post(&self, path: &str, query: &str) -> Result<reqwest::Response> {
        let timestamp = unix_millis()?;
        let unsigned = format!("{query}&recvWindow={RECV_WINDOW_MS}&timestamp={timestamp}");
        let signature = sign(&self.config.api_secret, &unsigned);
        let url = format!(
            "{}{path}?{unsigned}&signature={signature}",
            self.config.base_url
        );
        self.http
            .post(url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await
            .context("sending signed request to Binance")
    }
}

fn sign(secret: &str, payload: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn unix_millis() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is set before the Unix epoch"))?
        .as_millis();
    Ok(millis as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From memory of Binance's own API docs "Signed Endpoint Examples"
    /// (secret `"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j"`,
    /// query string below) — this session has no network access to
    /// re-fetch and confirm it against the live docs, so treat a future
    /// mismatch here as reason to re-derive the vector from Binance's
    /// current docs, not as proof this signing code regressed.
    #[test]
    fn sign_matches_binances_own_documented_vector() {
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        let payload = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&\
                        price=0.1&recvWindow=5000&timestamp=1499827319559";
        assert_eq!(
            sign(secret, payload),
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
    }
}
