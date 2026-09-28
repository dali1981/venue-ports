//! `BybitLive` — the second `CexLive` implementation, built alongside
//! Binance rather than after it. Pointing `BybitConfig::base_url` at
//! Bybit's testnet host (the default) makes this the "Simulated" mode for
//! the Bybit leg, per §3.
//!
//! **Adapter conventions, flagged rather than guessed** — no sandbox
//! account exists yet to confirm either against a real response
//! (Blocker 3, `IMPLEMENTATION_PLAN.md`):
//! - Like `BinanceLive`, the `LOT_SIZE` step per symbol is supplied by the
//!   caller, not fetched from `GET /v5/instruments-info`.
//! - Bybit's own placement response never carries fill details, so this
//!   adapter always follows up with `GET /v5/order/realtime`, polling a
//!   bounded number of times until the order reaches a terminal status.
//! - The commission asset per symbol is also supplied by the caller
//!   (Bybit's order-status response reports a fee amount but not its
//!   currency); this is the one convention closest to a guess, since which
//!   asset a spot fee is actually charged in depends on account-level fee
//!   settings this adapter cannot see.

use crate::cex::bybit::rest::{BybitOrder, BybitRest};
use crate::cex::{CexExecutor, CexFill, OrderRequest, OrderSide};
use crate::Provenance;
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::Duration;

/// How many times to re-poll `GET /v5/order/realtime` before giving up on
/// an order that never reached a terminal status — a market order should
/// settle in well under this.
const MAX_POLL_ATTEMPTS: u32 = 10;
const POLL_INTERVAL: Duration = Duration::from_millis(200);

pub struct BybitLive {
    rest: BybitRest,
    step_size: HashMap<String, Decimal>,
    commission_asset: HashMap<String, String>,
}

impl BybitLive {
    pub fn new(
        rest: BybitRest,
        step_size: HashMap<String, Decimal>,
        commission_asset: HashMap<String, String>,
    ) -> Self {
        Self {
            rest,
            step_size,
            commission_asset,
        }
    }

    fn round_to_step(&self, symbol: &str, quantity: Decimal) -> Result<Decimal> {
        let step = self.step_size.get(symbol).ok_or_else(|| {
            anyhow!("no LOT_SIZE step configured for symbol {symbol} — refusing to send an unrounded quantity")
        })?;
        if step.is_zero() {
            bail!("step size for {symbol} is zero");
        }
        Ok((quantity / step).floor() * step)
    }

    async fn poll_until_terminal(&self, symbol: &str, order_id: &str) -> Result<BybitOrder> {
        for _ in 0..MAX_POLL_ATTEMPTS {
            if let Some(order) = self.rest.get_order(symbol, order_id).await? {
                if is_terminal(&order.order_status) {
                    return Ok(order);
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        bail!("order {order_id} did not reach a terminal status after {MAX_POLL_ATTEMPTS} polls");
    }
}

/// Bybit V5 order-status terminal states — see
/// <https://bybit-exchange.github.io/docs/v5/enum#orderstatus>.
fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "Filled" | "Cancelled" | "Rejected" | "PartiallyFilledCanceled"
    )
}

#[async_trait]
impl CexExecutor for BybitLive {
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill> {
        if req.reduce_only {
            bail!(
                "reduce_only is not supported on Bybit spot, which holds no positions — \
                 refusing to send {} as an unguarded order",
                req.symbol
            );
        }
        let side = match req.side {
            OrderSide::Buy => "Buy",
            OrderSide::Sell => "Sell",
        };
        let quantity = self.round_to_step(&req.symbol, req.quantity)?;
        if quantity.is_zero() {
            bail!(
                "quantity {} rounded down to zero at {}'s step size",
                req.quantity,
                req.symbol
            );
        }

        let order_id = self
            .rest
            .place_market_order(&req.symbol, side, quantity)
            .await?;
        let order = self.poll_until_terminal(&req.symbol, &order_id).await?;
        self.fill_from_order(&req.symbol, order_id, order)
    }

    fn label(&self) -> &'static str {
        "bybit-live"
    }
}

impl BybitLive {
    fn fill_from_order(
        &self,
        symbol: &str,
        order_id: String,
        order: BybitOrder,
    ) -> Result<CexFill> {
        if order.cum_exec_qty.is_zero() {
            bail!(
                "order {order_id} ended in status {} with no fill",
                order.order_status
            );
        }
        let filled_price: Decimal = order
            .avg_price
            .parse()
            .map_err(|_| anyhow!("could not parse avgPrice {:?}", order.avg_price))?;
        let commission_asset = self
            .commission_asset
            .get(symbol)
            .cloned()
            .unwrap_or_default();

        Ok(CexFill {
            filled_qty: order.cum_exec_qty,
            filled_price,
            commission: order.cum_exec_fee,
            commission_asset,
            provenance: Provenance::Landed,
            order_ref: order_id.parse().ok(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::bybit::rest::BybitConfig;
    use std::str::FromStr;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn decimal(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn request() -> OrderRequest {
        OrderRequest {
            symbol: "SOLUSDT".to_string(),
            side: OrderSide::Buy,
            quantity: decimal("10.037"),
            quoted_price: decimal("150"),
            reduce_only: false,
        }
    }

    fn live(server: &MockServer) -> BybitLive {
        let rest = BybitRest::new(BybitConfig {
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            api_secret: "test-secret".to_string(),
        });
        BybitLive::new(
            rest,
            HashMap::from([("SOLUSDT".to_string(), decimal("0.01"))]),
            HashMap::from([("SOLUSDT".to_string(), "USDT".to_string())]),
        )
    }

    #[tokio::test]
    async fn a_filled_order_becomes_a_landed_fill_after_polling_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v5/order/create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 0, "retMsg": "OK",
                "result": {"orderId": "9001", "orderLinkId": ""},
                "retExtInfo": {}, "time": 1
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v5/order/realtime"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 0, "retMsg": "OK",
                "result": {"list": [{
                    "orderStatus": "Filled",
                    "avgPrice": "150.1",
                    "cumExecQty": "10.03",
                    "cumExecFee": "0.02"
                }]},
                "retExtInfo": {}, "time": 2
            })))
            .mount(&server)
            .await;

        let adapter = live(&server);
        let fill = adapter.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("10.03"));
        assert_eq!(fill.filled_price, decimal("150.1"));
        assert_eq!(fill.commission, decimal("0.02"));
        assert_eq!(fill.commission_asset, "USDT");
        assert_eq!(fill.provenance, Provenance::Landed);
        assert_eq!(fill.order_ref, Some(9001));
    }

    #[tokio::test]
    async fn a_rejected_order_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v5/order/create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 0, "retMsg": "OK",
                "result": {"orderId": "9002", "orderLinkId": ""},
                "retExtInfo": {}, "time": 1
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v5/order/realtime"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 0, "retMsg": "OK",
                "result": {"list": [{
                    "orderStatus": "Rejected",
                    "avgPrice": "0",
                    "cumExecQty": "0",
                    "cumExecFee": "0"
                }]},
                "retExtInfo": {}, "time": 2
            })))
            .mount(&server)
            .await;

        let adapter = live(&server);
        let err = adapter.execute(&request()).await.unwrap_err();
        assert!(err.to_string().contains("Rejected"));
    }

    #[tokio::test]
    async fn reduce_only_is_refused_before_anything_is_sent() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let adapter = live(&server);
        crate::testkit::contract::cex_spot_rejects_reduce_only(
            &adapter,
            crate::testkit::contract::CexContractFixture { request: request() },
        )
        .await;
        // `expect(0)` is verified when `server` drops.
    }
}
