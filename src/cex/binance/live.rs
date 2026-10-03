//! `BinanceLive` — the first `CexLive` implementation (`SPEC.md` §6,
//! `IMPLEMENTATION_PLAN.md` Phase 7). Pointing `BinanceConfig::base_url` at
//! `testnet.binance.vision` (the default) makes this the "Simulated" mode
//! for the Binance leg, per §3 — there is deliberately no separate struct.
//!
//! **An `Err` means nothing filled, unless it is an `OrderStateUnknown`**
//! (`SPEC.md` §6). To keep to that:
//!
//! - Every order carries a `newClientOrderId` generated for that call, so
//!   the venue can be asked about it when the answer to the placing call
//!   is lost.
//! - Timestamps come from the venue's clock (`GET /api/v3/time`), read
//!   before the first signed call, every 10 minutes after, and again on a
//!   `-1021` (README defect 3).
//! - A lost answer — a timeout, a dropped connection, an HTTP 5xx, which
//!   Binance documents as "execution status unknown" — is followed by
//!   `GET /api/v3/order?origClientOrderId=`. Found: it is read like any
//!   other answer. "No such order" (`-2013`) once `recvWindow` has passed:
//!   the venue never accepted the order, a plain error. The query failing
//!   until the adapter stops waiting: `OrderStateUnknown`.
//! - A market order that partly filled and then expired (`EXPIRED`,
//!   `CANCELED`, … with a non-zero `executedQty`) is a partial `CexFill`,
//!   never an error (README defect 4).
//! - Commission is read from the order's trade lines: the `fills` of the
//!   placing call's `FULL` answer, or `GET /api/v3/myTrades?orderId=` when
//!   the order was found by a status query, which carries none. Commission
//!   charged in more than one asset (BNB and the quote asset in one order)
//!   cannot go in a `CexFill`, so it is `OrderStateUnknown` with that
//!   explanation, never a fill that drops the second asset (README
//!   defect 2).
//!
//! **Adapter convention, flagged for the same reason `EvmSimulated`'s
//! payload convention is:** the step size a quantity must be rounded to is
//! supplied by the caller (`step_size`), not fetched from Binance's own
//! `GET /api/v3/exchangeInfo`. Auto-fetching and caching per-symbol filters
//! is a reasonable follow-up, not required to place a correctly-rounded
//! order today.
//!
//! Not yet confirmed against the Spot Testnet: that Binance reports a
//! market order that ran out of book as `EXPIRED` with its partial `fills`
//! (README defect 4 asks for this check).

use crate::cex::binance::order::{settled, single_commission, trade_lines, unreadable_fill};
use crate::cex::binance::rest::{
    BinanceOrderResponse, BinanceRest, OrderCheck, MY_TRADES_PATH, ORDER_PATH,
};
use crate::cex::{new_client_order_id, CexExecutor, CexFill, OrderRequest, OrderSide};
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

    /// Reads an order the venue reports in a terminal state into a fill.
    async fn settle(
        &self,
        req: &OrderRequest,
        client_order_id: &str,
        order: BinanceOrderResponse,
    ) -> Result<CexFill> {
        if order.executed_qty.is_zero() {
            bail!(
                "order {} ended in status {} with nothing filled",
                order.order_id,
                order.status
            );
        }
        let unknown = |why| {
            unreadable_fill(
                &req.symbol,
                client_order_id,
                order.order_id,
                order.executed_qty,
                why,
            )
        };

        let listed: Decimal = order.fills.iter().map(|fill| fill.qty).sum();
        let lines = if listed == order.executed_qty {
            order.fills
        } else {
            trade_lines(
                self.rest.client(),
                MY_TRADES_PATH,
                &req.symbol,
                order.order_id,
                order.executed_qty,
            )
            .await
            .map_err(unknown)?
        };
        let (commission, commission_asset) = single_commission(&lines).map_err(unknown)?;
        let notional: Decimal = lines.iter().map(|line| line.price * line.qty).sum();

        Ok(CexFill {
            filled_qty: order.executed_qty,
            filled_price: notional / order.executed_qty,
            commission,
            commission_asset,
            provenance: Provenance::Landed,
            order_ref: Some(order.order_id),
            client_order_id: Some(client_order_id.to_string()),
            venue_time_ms: order.transact_time.or(order.update_time),
            trades: lines.iter().map(|line| line.trade()).collect(),
        })
    }

    /// The side and the venue-ready quantity of `req`, or why it is refused
    /// before anything is sent: `execute` and `test_order` refuse the same
    /// orders.
    fn market_order(&self, req: &OrderRequest) -> Result<(&'static str, Decimal)> {
        if req.reduce_only {
            bail!(
                "reduce_only is not supported on Binance spot, which holds no positions — \
                 refusing to send {} as an unguarded order",
                req.symbol
            );
        }
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
        Ok((side, quantity))
    }

    /// Checks `req` as [`CexExecutor::execute`] would place it, at
    /// `POST /api/v3/order/test` with `computeCommissionRates=true`: the
    /// venue validates the order and states the commission its trades would
    /// pay. Nothing is sent to the matching engine, so nothing can trade and
    /// any failure, a lost answer included, is a plain error.
    pub async fn test_order(&self, req: &OrderRequest) -> Result<OrderCheck> {
        let (side, quantity) = self.market_order(req)?;
        let client_order_id = new_client_order_id();
        let rates = self
            .rest
            .test_market_order(&req.symbol, side, quantity, &client_order_id)
            .await
            .map_err(|err| {
                anyhow!(
                    "the test of {side} {quantity} {} ({client_order_id}) failed: {err}",
                    req.symbol
                )
            })?;
        Ok(OrderCheck {
            quantity,
            client_order_id,
            rates,
        })
    }
}

#[async_trait]
impl CexExecutor for BinanceLive {
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill> {
        let (side, quantity) = self.market_order(req)?;
        let client_order_id = new_client_order_id();
        let what = format!("{side} {quantity} {} ({client_order_id})", req.symbol);
        let placed = self
            .rest
            .place_market_order(&req.symbol, side, quantity, &client_order_id)
            .await;
        let order = settled(
            self.rest.client(),
            ORDER_PATH,
            &req.symbol,
            &client_order_id,
            &what,
            placed,
        )
        .await?;
        self.settle(req, &client_order_id, order).await
    }

    fn label(&self) -> &'static str {
        "binance-live"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance::rest::BinanceConfig;
    use crate::cex::{CexTimings, CexTrade, MakerTaker, OrderStateUnknown};
    use std::str::FromStr;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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

    fn fast() -> CexTimings {
        CexTimings {
            request_timeout: Duration::from_millis(200),
            recv_window: Duration::from_millis(300),
            clock_refresh: Duration::from_secs(600),
            poll_interval: Duration::from_millis(10),
            poll_timeout: Duration::from_millis(300),
            trades_timeout: Duration::from_millis(200),
        }
    }

    fn live(server: &MockServer, step_size: Decimal) -> BinanceLive {
        let rest = BinanceRest::with_timings(
            BinanceConfig {
                base_url: server.uri(),
                api_key: "test-key".to_string(),
                api_secret: "test-secret".to_string(),
            },
            fast(),
        );
        BinanceLive::new(rest, HashMap::from([("SOLUSDT".to_string(), step_size)]))
    }

    /// A venue with a clock. Every signed call reads it first.
    async fn venue() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "serverTime": local_now_ms() })),
            )
            .mount(&server)
            .await;
        server
    }

    fn param(request: &Request, key: &str) -> Option<String> {
        request
            .url
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }

    fn filled_order(status: &str, executed: &str) -> serde_json::Value {
        serde_json::json!({
            "orderId": 42,
            "status": status,
            "executedQty": executed,
        })
    }

    async fn mount_placing(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(response)
            .mount(server)
            .await;
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
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 42,
                "status": "FILLED",
                "executedQty": "10.03",
                "fills": [
                    {"price": "150.0", "qty": "5.00", "commission": "0.01", "commissionAsset": "USDT"},
                    {"price": "150.2", "qty": "5.03", "commission": "0.01", "commissionAsset": "USDT"}
                ]
            })),
        )
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
    async fn every_order_carries_its_own_client_order_id_and_the_venues_clock() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 42, "status": "FILLED", "executedQty": "10.03",
                "fills": [{"price": "150", "qty": "10.03", "commission": "0.01", "commissionAsset": "USDT"}]
            })),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        adapter.execute(&request()).await.unwrap();
        adapter.execute(&request()).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let orders: Vec<&Request> = requests
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .collect();
        let ids: Vec<String> = orders
            .iter()
            .map(|r| param(r, "newClientOrderId").unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(param(orders[0], "quantity").unwrap(), "10.03");
        assert_eq!(param(orders[0], "newOrderRespType").unwrap(), "FULL");
        // The clock is read once, before the first signed call.
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path() == "/api/v3/time")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_rejected_order_is_an_error() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 7,
                "status": "REJECTED",
                "executedQty": "0",
                "fills": []
            })),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        let err = adapter.execute(&request()).await.unwrap_err();
        assert!(err.to_string().contains("REJECTED"));
        assert!(err.downcast_ref::<OrderStateUnknown>().is_none());
    }

    #[tokio::test]
    async fn a_refusal_is_a_plain_error_carrying_the_venues_code() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -2010, "msg": "Account has insufficient balance for requested action."
            })),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        let err = adapter.execute(&request()).await.unwrap_err();

        assert!(err.downcast_ref::<OrderStateUnknown>().is_none());
        let message = err.to_string();
        assert!(message.contains("-2010"), "{message}");
        assert!(message.contains("insufficient balance"), "{message}");
    }

    #[tokio::test]
    async fn a_market_order_that_expired_part_filled_is_a_partial_fill() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 42,
                "status": "EXPIRED",
                "executedQty": "4.00",
                "fills": [
                    {"price": "150", "qty": "4.00", "commission": "0.006", "commissionAsset": "USDT"}
                ]
            })),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        let fill = adapter.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("4.00"));
        assert_eq!(fill.filled_price, decimal("150"));
        assert_eq!(fill.commission, decimal("0.006"));
        assert_eq!(fill.order_ref, Some(42));
    }

    #[tokio::test]
    async fn commission_in_two_assets_is_order_state_unknown() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 42,
                "status": "FILLED",
                "executedQty": "10.03",
                "fills": [
                    {"price": "150", "qty": "5", "commission": "0.0001", "commissionAsset": "BNB"},
                    {"price": "150", "qty": "5.03", "commission": "0.01", "commissionAsset": "USDT"}
                ]
            })),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        let err = adapter.execute(&request()).await.unwrap_err();

        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown, not a fill with a wrong commission");
        assert_eq!(unknown.order_ref, Some(42));
        assert_eq!(unknown.symbol, "SOLUSDT");
        assert!(
            format!("{err:#}").contains("more than one asset"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_lost_answer_is_recovered_by_the_client_order_id() {
        let server = venue().await;
        // The venue takes the order, but its answer never arrives in time.
        mount_placing(
            &server,
            ResponseTemplate::new(200)
                .set_body_json(filled_order("FILLED", "10.03"))
                .set_delay(Duration::from_millis(600)),
        )
        .await;
        // The status query carries `updateTime` and no `transactTime`, and
        // `myTrades` names each trade `id`.
        let mut queried = filled_order("FILLED", "10.03");
        queried["updateTime"] = serde_json::json!(1_700_000_000_123u64);
        Mock::given(method("GET"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(queried))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/myTrades"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": 901, "orderId": 42, "price": "150", "qty": "10", "commission": "0.015", "commissionAsset": "USDT"},
                {"id": 902, "orderId": 42, "price": "151", "qty": "0.03", "commission": "0.00005", "commissionAsset": "USDT"}
            ])))
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        let fill = adapter.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("10.03"));
        assert_eq!(fill.commission, decimal("0.01505"));
        assert_eq!(fill.order_ref, Some(42));
        assert_eq!(fill.venue_time_ms, Some(1_700_000_000_123));
        assert_eq!(
            fill.trades.iter().map(|t| t.trade_id).collect::<Vec<_>>(),
            vec![Some(901), Some(902)]
        );
        crate::testkit::contract::assert_fill_shape(&fill, adapter.label());

        let requests = server.received_requests().await.unwrap();
        let sent = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .unwrap();
        let asked = requests
            .iter()
            .find(|r| r.method.as_str() == "GET" && r.url.path() == "/api/v3/order")
            .unwrap();
        assert_eq!(
            param(asked, "origClientOrderId"),
            param(sent, "newClientOrderId")
        );
        assert_eq!(fill.client_order_id, param(sent, "newClientOrderId"));
    }

    #[tokio::test]
    async fn a_lost_answer_and_a_failed_query_is_order_state_unknown() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200)
                .set_body_json(filled_order("FILLED", "10.03"))
                .set_delay(Duration::from_millis(600)),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        let err = adapter.execute(&request()).await.unwrap_err();

        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown");
        assert_eq!(unknown.symbol, "SOLUSDT");
        assert_eq!(unknown.order_ref, None);
        assert!(unknown.client_order_id.starts_with("vp-"));
    }

    #[tokio::test]
    async fn a_5xx_and_no_such_order_after_recv_window_is_a_plain_error() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(503).set_body_string("Service Unavailable."),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -2013, "msg": "Order does not exist."
            })))
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        let started = std::time::Instant::now();
        let err = adapter.execute(&request()).await.unwrap_err();

        assert!(err.downcast_ref::<OrderStateUnknown>().is_none(), "{err:#}");
        assert!(err.to_string().contains("never accepted"), "{err}");
        // "No such order" only counts once recvWindow has passed.
        assert!(started.elapsed() >= fast().recv_window);
    }

    #[tokio::test]
    async fn a_part_filled_answer_is_polled_until_the_order_settles() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 42, "status": "PARTIALLY_FILLED", "executedQty": "5",
                "fills": [{"price": "150", "qty": "5", "commission": "0.01", "commissionAsset": "USDT"}]
            })),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(filled_order("FILLED", "10.03")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/myTrades"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"orderId": 42, "price": "150", "qty": "5", "commission": "0.01", "commissionAsset": "USDT"},
                {"orderId": 42, "price": "150", "qty": "5.03", "commission": "0.01", "commissionAsset": "USDT"}
            ])))
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        let fill = adapter.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("10.03"));
        assert_eq!(fill.commission, decimal("0.02"));
    }

    #[tokio::test]
    async fn reduce_only_is_refused_before_anything_is_sent() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        crate::testkit::contract::cex_spot_rejects_reduce_only(
            &adapter,
            crate::testkit::contract::CexContractFixture { request: request() },
        )
        .await;
        // `expect(0)` is verified when `server` drops.
    }

    fn placed_with_its_trades() -> serde_json::Value {
        serde_json::json!({
            "orderId": 42, "status": "FILLED", "executedQty": "10.03", "transactTime": 1_507_725_176_595u64,
            "fills": [
                {"price": "150.0", "qty": "5.00", "commission": "0.01", "commissionAsset": "USDT", "tradeId": 56},
                {"price": "150.2", "qty": "5.03", "commission": "0.01", "commissionAsset": "USDT", "tradeId": 57}
            ]
        })
    }

    #[tokio::test]
    async fn a_landed_fill_names_the_client_order_id_sent_the_venues_time_and_each_trade() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(placed_with_its_trades()),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        let fill = adapter.execute(&request()).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let sent = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .unwrap();
        assert_eq!(fill.client_order_id, param(sent, "newClientOrderId"));
        assert_eq!(fill.venue_time_ms, Some(1_507_725_176_595));
        assert_eq!(
            fill.trades,
            vec![
                CexTrade {
                    trade_id: Some(56),
                    price: decimal("150.0"),
                    qty: decimal("5.00"),
                    commission: decimal("0.01"),
                    commission_asset: "USDT".into()
                },
                CexTrade {
                    trade_id: Some(57),
                    price: decimal("150.2"),
                    qty: decimal("5.03"),
                    commission: decimal("0.01"),
                    commission_asset: "USDT".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn binance_spot_passes_the_cex_contract() {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(placed_with_its_trades()),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        crate::testkit::contract::cex_executor_contract(
            &adapter,
            crate::testkit::contract::CexContractFixture { request: request() },
        )
        .await;
    }

    async fn mount_order_test(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/api/v3/order/test"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn an_order_test_sends_the_same_order_with_commission_rates_asked_and_reads_them() {
        let server = venue().await;
        mount_order_test(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "standardCommissionForOrder": {"maker": "0.00100000", "taker": "0.00100000"},
                "specialCommissionForOrder": {"maker": "0.00000000", "taker": "0.00000000"},
                "taxCommissionForOrder": {"maker": "0.00000000", "taker": "0.00000000"},
                "discount": {"enabledForAccount": true, "enabledForSymbol": true, "discountAsset": "BNB", "discount": "0.25000000"}
            })),
        )
        .await;

        let adapter = live(&server, decimal("0.01"));
        let check = adapter.test_order(&request()).await.unwrap();

        assert_eq!(check.quantity, decimal("10.03"));
        assert_eq!(
            check.rates.standard,
            MakerTaker {
                maker: decimal("0.001"),
                taker: decimal("0.001")
            }
        );
        assert_eq!(
            check.rates.special,
            Some(MakerTaker {
                maker: Decimal::ZERO,
                taker: Decimal::ZERO
            })
        );
        assert_eq!(check.rates.tax.taker, Decimal::ZERO);
        let discount = check.rates.discount.unwrap();
        assert_eq!(
            (
                discount.asset.as_str(),
                discount.rate,
                discount.enabled_for_account
            ),
            ("BNB", decimal("0.25"), true)
        );

        let requests = server.received_requests().await.unwrap();
        let tested = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .unwrap();
        assert_eq!(tested.url.path(), "/api/v3/order/test", "nothing is placed");
        assert_eq!(param(tested, "computeCommissionRates").unwrap(), "true");
        assert_eq!(param(tested, "type").unwrap(), "MARKET");
        assert_eq!(param(tested, "side").unwrap(), "BUY");
        assert_eq!(param(tested, "quantity").unwrap(), "10.03");
        assert_eq!(
            param(tested, "newClientOrderId"),
            Some(check.client_order_id)
        );
    }

    #[tokio::test]
    async fn an_order_test_without_special_or_discount_rates_still_reads() {
        let server = venue().await;
        mount_order_test(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "standardCommissionForOrder": {"maker": "0.00000112", "taker": "0.00000114"},
                "taxCommissionForOrder": {"maker": "0.00000112", "taker": "0.00000114"}
            })),
        )
        .await;

        let check = live(&server, decimal("0.01"))
            .test_order(&request())
            .await
            .unwrap();
        assert_eq!((check.rates.special, check.rates.discount), (None, None));
        assert_eq!(check.rates.standard.taker, decimal("0.00000114"));
    }

    #[tokio::test]
    async fn an_order_test_the_venue_refuses_is_an_error_carrying_its_code() {
        let server = venue().await;
        mount_order_test(
            &server,
            ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"code": -1013, "msg": "Filter failure: NOTIONAL"}),
            ),
        )
        .await;

        let err = live(&server, decimal("0.01"))
            .test_order(&request())
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("-1013") && text.contains("NOTIONAL"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_order_test_refuses_what_execute_refuses_before_sending() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let adapter = live(&server, decimal("0.01"));
        let reduce_only = OrderRequest {
            reduce_only: true,
            ..request()
        };
        assert!(adapter
            .test_order(&reduce_only)
            .await
            .unwrap_err()
            .to_string()
            .contains("reduce_only"));
        let dust = OrderRequest {
            quantity: decimal("0.001"),
            ..request()
        };
        assert!(adapter
            .test_order(&dust)
            .await
            .unwrap_err()
            .to_string()
            .contains("rounded down to zero"));
        // `expect(0)` is verified when `server` drops.
    }
}
