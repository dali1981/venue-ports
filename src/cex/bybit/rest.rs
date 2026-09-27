//! A signed REST client for Bybit's V5 unified-trading API.
//!
//! Every private call carries four headers: `X-BAPI-API-KEY`,
//! `X-BAPI-TIMESTAMP`, `X-BAPI-RECV-WINDOW`, and `X-BAPI-SIGN` — an
//! HMAC-SHA256 over `timestamp + api_key + recv_window + payload`, where
//! `payload` is the exact query string (GET) or JSON body (POST) sent, hex
//! encoded. See <https://bybit-exchange.github.io/docs/v5/guide#authentication>.

use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Bybit's own tolerance window for how stale a signed request's timestamp
/// may be, in milliseconds.
const RECV_WINDOW_MS: &str = "5000";

pub struct BybitConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_secret: String,
}

impl BybitConfig {
    /// Reads `BYBIT_API_KEY` and `BYBIT_API_SECRET` (required) and
    /// `BYBIT_BASE_URL` (optional, defaults to the **testnet** host — a
    /// production run opts in explicitly, never by default).
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            base_url: std::env::var("BYBIT_BASE_URL")
                .unwrap_or_else(|_| "https://api-testnet.bybit.com".to_string()),
            api_key: std::env::var("BYBIT_API_KEY").context("BYBIT_API_KEY is not set")?,
            api_secret: std::env::var("BYBIT_API_SECRET").context("BYBIT_API_SECRET is not set")?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(rename = "retCode")]
    ret_code: i64,
    #[serde(rename = "retMsg")]
    ret_msg: String,
    result: T,
}

#[derive(Debug, Deserialize)]
pub struct CreateOrderResult {
    #[serde(rename = "orderId")]
    pub order_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BybitOrder {
    #[serde(rename = "orderStatus")]
    pub order_status: String,
    #[serde(rename = "avgPrice")]
    pub avg_price: String,
    #[serde(rename = "cumExecQty")]
    pub cum_exec_qty: Decimal,
    #[serde(rename = "cumExecFee")]
    pub cum_exec_fee: Decimal,
}

#[derive(Debug, Deserialize)]
struct OrderList {
    list: Vec<BybitOrder>,
}

pub struct BybitRest {
    config: BybitConfig,
    http: reqwest::Client,
}

impl BybitRest {
    pub fn new(config: BybitConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    /// Places a spot market order. `side` is `"Buy"` or `"Sell"`. Bybit's
    /// own placement response carries only the order ID — never fill
    /// details (unlike Binance) — so a caller must always follow up with
    /// [`Self::get_order`], per `SPEC.md` §6's "fall back to a status
    /// query only when the placing response is ambiguous": here, it always
    /// is.
    pub async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        qty: Decimal,
    ) -> Result<String> {
        let body = json!({
            "category": "spot",
            "symbol": symbol,
            "side": side,
            "orderType": "Market",
            "qty": qty.to_string(),
        })
        .to_string();

        let response = self.signed_post("/v5/order/create", &body).await?;
        let envelope: Envelope<CreateOrderResult> = response
            .json()
            .await
            .context("decoding Bybit order response")?;
        if envelope.ret_code != 0 {
            bail!(
                "Bybit rejected the order: {} ({})",
                envelope.ret_msg,
                envelope.ret_code
            );
        }
        Ok(envelope.result.order_id)
    }

    /// Reads back the current state of a placed order.
    pub async fn get_order(&self, symbol: &str, order_id: &str) -> Result<Option<BybitOrder>> {
        let query = format!("category=spot&symbol={symbol}&orderId={order_id}");
        let response = self.signed_get("/v5/order/realtime", &query).await?;
        let envelope: Envelope<OrderList> = response
            .json()
            .await
            .context("decoding Bybit order-status response")?;
        if envelope.ret_code != 0 {
            bail!(
                "Bybit rejected the order-status query: {} ({})",
                envelope.ret_msg,
                envelope.ret_code
            );
        }
        Ok(envelope.result.list.into_iter().next())
    }

    async fn signed_post(&self, path: &str, body: &str) -> Result<reqwest::Response> {
        let timestamp = unix_millis()?;
        let signature = sign(
            &self.config.api_secret,
            &timestamp,
            &self.config.api_key,
            body,
        );
        self.http
            .post(format!("{}{path}", self.config.base_url))
            .header("X-BAPI-API-KEY", &self.config.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW_MS)
            .header("X-BAPI-SIGN", signature)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context("sending signed POST to Bybit")
    }

    async fn signed_get(&self, path: &str, query: &str) -> Result<reqwest::Response> {
        let timestamp = unix_millis()?;
        let signature = sign(
            &self.config.api_secret,
            &timestamp,
            &self.config.api_key,
            query,
        );
        self.http
            .get(format!("{}{path}?{query}", self.config.base_url))
            .header("X-BAPI-API-KEY", &self.config.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW_MS)
            .header("X-BAPI-SIGN", signature)
            .send()
            .await
            .context("sending signed GET to Bybit")
    }
}

fn sign(secret: &str, timestamp: &u64, api_key: &str, payload: &str) -> String {
    let signable = format!("{timestamp}{api_key}{RECV_WINDOW_MS}{payload}");
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(signable.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn unix_millis() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock is set before the Unix epoch"))?
        .as_millis();
    Ok(millis as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No real credentials to sign against yet (Blocker 3,
    /// `IMPLEMENTATION_PLAN.md`), so this pins the exact signable-string
    /// construction (`timestamp + api_key + recv_window + payload`, per
    /// Bybit's V5 auth docs) against a manually recomputed HMAC, to catch a
    /// future regression even without a public vector.
    #[test]
    fn sign_builds_the_documented_signable_string() {
        let secret = "test-secret";
        let got = sign(
            secret,
            &1658384314791u64,
            "test-key",
            "category=spot&symbol=BTCUSDT",
        );
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(b"1658384314791test-key5000category=spot&symbol=BTCUSDT");
        let expected = hex::encode(mac.finalize().into_bytes());
        assert_eq!(got, expected);
    }
}
