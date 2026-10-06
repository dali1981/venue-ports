//! A signed REST client for Bybit's V5 unified-trading API.
//!
//! Every private call carries four headers: `X-BAPI-API-KEY`,
//! `X-BAPI-TIMESTAMP`, `X-BAPI-RECV-WINDOW`, and `X-BAPI-SIGN` — an
//! HMAC-SHA256 over `timestamp + api_key + recv_window + payload`, where
//! `payload` is the exact query string (GET) or JSON body (POST) sent, hex
//! encoded. See <https://bybit-exchange.github.io/docs/v5/guide#authentication>.
//!
//! A placing call that fails is sorted by whether Bybit may have acted on
//! it ([`PlaceError`]), because `BybitLive` must tell "refused, nothing
//! filled" from "may have filled" (`SPEC.md` §6).

use crate::cex::CexTimings;
use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, Mac};
use reqwest::StatusCode;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Bybit's own tolerance window for how stale a signed request's timestamp
/// may be, in milliseconds.
const RECV_WINDOW_MS: &str = "5000";

/// Non-zero `retCode`s that do not say Bybit refused the order: `10000`
/// "Server Timeout" and `10016` "Server error". After either, the order
/// may still have been placed, so the answer counts as lost and the order
/// is looked up. Taken from Bybit's V5 error-code table and not yet seen
/// on a real order; treating a refusal as lost costs one status query,
/// while the other way round would hide a fill.
const AMBIGUOUS_RET_CODES: [i64; 2] = [10_000, 10_016];

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
struct Envelope {
    #[serde(rename = "retCode")]
    ret_code: i64,
    #[serde(rename = "retMsg")]
    ret_msg: String,
    /// `{}` on an error, so it is read only once `retCode` is 0.
    #[serde(default)]
    result: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CreateOrderResult {
    #[serde(rename = "orderId")]
    order_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BybitOrder {
    #[serde(rename = "orderId")]
    pub(crate) order_id: String,
    #[serde(rename = "orderStatus")]
    pub(crate) order_status: String,
    #[serde(rename = "avgPrice")]
    pub(crate) avg_price: String,
    #[serde(rename = "cumExecQty")]
    pub(crate) cum_exec_qty: Decimal,
    #[serde(rename = "cumExecFee")]
    pub(crate) cum_exec_fee: Decimal,
    /// When the order last changed, ms since the epoch as a decimal string.
    #[serde(rename = "updatedTime", default)]
    pub(crate) updated_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrderList {
    list: Vec<BybitOrder>,
}

/// Which id to look an order up by.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OrderKey<'a> {
    /// Bybit's own `orderId`, from a placing call that answered.
    Id(&'a str),
    /// The `orderLinkId` this crate gave the order, when the placing call's
    /// answer was lost.
    LinkId(&'a str),
}

/// A placing call that failed, sorted by whether Bybit may have acted on
/// it.
#[derive(Debug)]
pub(crate) enum PlaceError {
    /// Nothing reached Bybit.
    NotSent(anyhow::Error),
    /// Bybit answered with a refusal: nothing was placed.
    Refused(anyhow::Error),
    /// Sent, but no readable answer came back — a timeout, a dropped
    /// connection, an HTTP 5xx or 408, an ambiguous `retCode`, or a body
    /// that cannot be read. The order may have been placed.
    Lost(anyhow::Error),
}

pub struct BybitRest {
    config: BybitConfig,
    http: reqwest::Client,
    timings: CexTimings,
}

impl BybitRest {
    pub fn new(config: BybitConfig) -> Self {
        Self::with_timings(config, CexTimings::default())
    }

    /// As [`Self::new`], with every wait set by `timings`. Bybit reads
    /// `request_timeout`, `poll_interval` and `poll_timeout`.
    pub fn with_timings(config: BybitConfig, timings: CexTimings) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            timings,
        }
    }

    pub(crate) fn timings(&self) -> &CexTimings {
        &self.timings
    }

    /// Places a spot market order under `order_link_id`. `side` is `"Buy"`
    /// or `"Sell"`. Bybit's own placement response carries only the order
    /// ID — never fill details (unlike Binance) — so a caller must always
    /// follow up with [`Self::get_order`], per `SPEC.md` §6's "fall back to
    /// a status query only when the placing response is ambiguous": here, it
    /// always is.
    pub(crate) async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        qty: Decimal,
        order_link_id: &str,
    ) -> Result<String, PlaceError> {
        let body = json!({
            "category": "spot",
            "symbol": symbol,
            "side": side,
            "orderType": "Market",
            "qty": qty.normalize().to_string(),
            "orderLinkId": order_link_id,
        })
        .to_string();

        let response = match self.signed_post("/v5/order/create", &body).await {
            Ok(response) => response,
            // Failing to open the connection happens before a byte is sent.
            Err(err) if err.is_connect() || err.is_builder() => {
                return Err(PlaceError::NotSent(
                    anyhow::Error::new(err).context("sending the order to Bybit"),
                ))
            }
            Err(err) => {
                return Err(PlaceError::Lost(
                    anyhow::Error::new(err).context("sending the order to Bybit"),
                ))
            }
        };
        let status = response.status();
        let text = response.text().await;
        if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
            return Err(PlaceError::Lost(anyhow!(
                "Bybit answered HTTP {status}: {}",
                text.unwrap_or_default()
            )));
        }
        if !status.is_success() {
            return Err(PlaceError::Refused(anyhow!(
                "Bybit refused the order (HTTP {status}): {}",
                text.unwrap_or_default()
            )));
        }
        let text = text.map_err(|err| {
            PlaceError::Lost(anyhow::Error::new(err).context("reading Bybit's answer"))
        })?;
        let envelope: Envelope = serde_json::from_str(&text).map_err(|err| {
            PlaceError::Lost(anyhow::Error::new(err).context(format!("decoding {text}")))
        })?;
        match envelope.ret_code {
            0 => serde_json::from_value::<CreateOrderResult>(envelope.result)
                .map(|result| result.order_id)
                .map_err(|err| {
                    PlaceError::Lost(anyhow::Error::new(err).context(format!("decoding {text}")))
                }),
            code if AMBIGUOUS_RET_CODES.contains(&code) => Err(PlaceError::Lost(anyhow!(
                "Bybit answered {} ({code})",
                envelope.ret_msg
            ))),
            code => Err(PlaceError::Refused(anyhow!(
                "Bybit rejected the order: {} ({code})",
                envelope.ret_msg
            ))),
        }
    }

    /// Reads back the current state of an order, or `None` when Bybit does
    /// not list it.
    pub(crate) async fn get_order(
        &self,
        symbol: &str,
        key: OrderKey<'_>,
    ) -> Result<Option<BybitOrder>> {
        let key = match key {
            OrderKey::Id(order_id) => format!("orderId={order_id}"),
            OrderKey::LinkId(order_link_id) => format!("orderLinkId={order_link_id}"),
        };
        let query = format!("category=spot&symbol={symbol}&{key}");
        let response = self.signed_get("/v5/order/realtime", &query).await?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "Bybit answered the order-status query with HTTP {status}: {}",
                response.text().await.unwrap_or_default()
            );
        }
        let envelope: Envelope = response
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
        let orders: OrderList = serde_json::from_value(envelope.result)
            .context("decoding Bybit order-status response")?;
        Ok(orders.list.into_iter().next())
    }

    async fn signed_post(&self, path: &str, body: &str) -> reqwest::Result<reqwest::Response> {
        let timestamp = unix_millis();
        let signature = sign(
            &self.config.api_secret,
            &timestamp,
            &self.config.api_key,
            body,
        );
        self.http
            .post(format!("{}{path}", self.config.base_url))
            .timeout(self.timings.request_timeout)
            .header("X-BAPI-API-KEY", &self.config.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW_MS)
            .header("X-BAPI-SIGN", signature)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
    }

    async fn signed_get(&self, path: &str, query: &str) -> Result<reqwest::Response> {
        let timestamp = unix_millis();
        let signature = sign(
            &self.config.api_secret,
            &timestamp,
            &self.config.api_key,
            query,
        );
        self.http
            .get(format!("{}{path}?{query}", self.config.base_url))
            .timeout(self.timings.request_timeout)
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

/// The local clock, in Unix ms. A clock before 1970 signs 0, which Bybit
/// refuses with its own error.
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
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
