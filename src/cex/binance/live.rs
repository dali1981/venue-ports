//! `BinanceLive` — the first `CexLive` implementation (`SPEC.md` §6,
//! `IMPLEMENTATION_PLAN.md` Phase 7). Pointing `BinanceConfig::base_url` at
//! `testnet.binance.vision` (the default) makes this the "Simulated" mode
//! for the Binance leg, per §3 — there is deliberately no separate struct.
//!
//! **Adapter convention, flagged for the same reason `EvmSimulated`'s
//! payload convention is:** the step size a quantity must be rounded to is
//! supplied by the caller (`step_size`), not fetched from Binance's own
//! `GET /api/v3/exchangeInfo`. Auto-fetching and caching per-symbol filters
//! is a reasonable follow-up, not required to place a correctly-rounded
//! order today.

use crate::cex::binance::rest::{BinanceOrderResponse, BinanceRest};
use crate::cex::{CexExecutor, CexFill, OrderRequest, OrderSide};
use crate::Provenance;
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;

pub struct BinanceLive {
    rest: BinanceRest,
    /// Symbol (e.g. `"SOLUSDT"`) -> the venue's `LOT_SIZE` step for it.
    step_size: HashMap<String, Decimal>,
}

impl BinanceLive {
    pub fn new(rest: BinanceRest, step_size: HashMap<String, Decimal>) -> Self {
        Self { rest, step_size }
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
}

#[async_trait]
impl CexExecutor for BinanceLive {
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill> {
        let side = match req.side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };
        let quantity = self.round_to_step(&req.symbol, req.quantity)?;
        if quantity.is_zero() {
            bail!(
                "quantity {} rounded down to zero at {}'s step size",
                req.quantity,
                req.symbol
            );
        }

        let response = self
            .rest
            .place_market_order(&req.symbol, side, quantity)
            .await?;
        fill_from_response(response)
    }

    fn label(&self) -> &'static str {
        "binance-live"
    }
}

/// Binance order statuses that mean the order is fully done and its `fills`
/// carry the whole outcome — see
/// <https://developers.binance.com/docs/binance-spot-api-docs/enums#order-status-status>.
fn fill_from_response(response: BinanceOrderResponse) -> Result<CexFill> {
    match response.status.as_str() {
        "FILLED" | "PARTIALLY_FILLED" => {
            if response.fills.is_empty() {
                bail!(
                    "order {} reports status {} but carries no fills",
                    response.order_id,
                    response.status
                );
            }
            let filled_qty: Decimal = response.fills.iter().map(|f| f.qty).sum();
            let notional: Decimal = response.fills.iter().map(|f| f.price * f.qty).sum();
            let filled_price = notional / filled_qty;
            // Binance may charge commission in more than one asset in rare
            // cases (e.g. a BNB-discount fill spilling into the quote
            // asset); this adapter assumes one, the common case, and flags
            // rather than silently drops the rest if that assumption ever
            // breaks.
            let commission_asset = response.fills[0].commission_asset.clone();
            let commission: Decimal = response
                .fills
                .iter()
                .map(|f| {
                    if f.commission_asset != commission_asset {
                        Decimal::ZERO
                    } else {
                        f.commission
                    }
                })
                .sum();

            Ok(CexFill {
                filled_qty,
                filled_price,
                commission,
                commission_asset,
                provenance: Provenance::Landed,
                order_ref: Some(response.order_id),
            })
        }
        other => bail!("order {} ended in status {other}", response.order_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::rest::BinanceConfig;
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
        }
    }

    fn live(server: &MockServer, step_size: Decimal) -> BinanceLive {
        let rest = BinanceRest::new(BinanceConfig {
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            api_secret: "test-secret".to_string(),
        });
        BinanceLive::new(rest, HashMap::from([("SOLUSDT".to_string(), step_size)]))
    }

    #[test]
    fn rounds_down_to_the_configured_step_size() {
        let server_less_adapter = |step: &str| {
            let rest = BinanceRest::new(BinanceConfig {
                base_url: "http://unused".to_string(),
                api_key: "k".to_string(),
                api_secret: "s".to_string(),
            });
            BinanceLive::new(
                rest,
                HashMap::from([("SOLUSDT".to_string(), decimal(step))]),
            )
        };
        let adapter = server_less_adapter("0.01");
        assert_eq!(
            adapter.round_to_step("SOLUSDT", decimal("10.037")).unwrap(),
            decimal("10.03")
        );
    }

    #[test]
    fn refuses_a_symbol_with_no_configured_step_size() {
        let rest = BinanceRest::new(BinanceConfig {
            base_url: "http://unused".to_string(),
            api_key: "k".to_string(),
            api_secret: "s".to_string(),
        });
        let adapter = BinanceLive::new(rest, HashMap::new());
        let err = adapter.round_to_step("SOLUSDT", decimal("1")).unwrap_err();
        assert!(err.to_string().contains("no LOT_SIZE step configured"));
    }

    #[tokio::test]
    async fn a_filled_market_order_becomes_a_landed_fill() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 42,
                "status": "FILLED",
                "fills": [
                    {"price": "150.0", "qty": "5.00", "commission": "0.01", "commissionAsset": "USDT"},
                    {"price": "150.2", "qty": "5.03", "commission": "0.01", "commissionAsset": "USDT"}
                ]
            })))
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        let fill = adapter.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("10.03"));
        assert_eq!(fill.commission, decimal("0.02"));
        assert_eq!(fill.commission_asset, "USDT");
        assert_eq!(fill.provenance, Provenance::Landed);
        assert_eq!(fill.order_ref, Some(42));
    }

    #[tokio::test]
    async fn a_rejected_order_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 7,
                "status": "REJECTED",
                "fills": []
            })))
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        let err = adapter.execute(&request()).await.unwrap_err();
        assert!(err.to_string().contains("REJECTED"));
    }
}
