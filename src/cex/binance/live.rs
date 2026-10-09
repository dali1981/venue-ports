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
//! **An order left unknown can be asked about again** ([`CexOrders`]): one status
//! query by the client order id the adapter gave it, with the trade lines of a
//! fill read as `execute` reads them. "No such order" is conclusive only after
//! `recvWindow` and the clock's error bound have passed since the request that
//! placed it was signed, so the adapter remembers when it signed each placing
//! request whose answer it lost, until that order's state has been read; before
//! then the order is `Open`.
//!
//! Not yet confirmed against the Spot Testnet: that Binance reports a
//! market order that ran out of book as `EXPIRED` with its partial `fills`
//! (README defect 4 asks for this check).

use crate::cex::binance::client::{ApiError, NO_SUCH_ORDER};
use crate::cex::binance::order::{
    is_terminal, read_order, settled, single_commission, trade_lines, unreadable_fill,
};
use crate::cex::binance::rest::{
    BinanceOrderResponse, BinanceRest, OrderCheck, MY_TRADES_PATH, ORDER_PATH,
};
use crate::cex::{
    new_client_order_id, with_provenance, CexExecutor, CexFill, CexOrders, OrderRequest, OrderSide,
    OrderState, OrderStateUnknown,
};
use crate::Provenance;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

pub struct BinanceLive {
    rest: BinanceRest,
    /// Symbol (e.g. `"SOLUSDT"`) -> the venue's `LOT_SIZE` step for it.
    step_size: HashMap<String, Decimal>,
    /// For each order that ended `OrderStateUnknown` after the answer to its
    /// placing request was lost: when that request was signed, which is what
    /// "no such order" can be told conclusive against. Removed once the order's
    /// state has been read.
    lost: Mutex<HashMap<String, Instant>>,
}

impl BinanceLive {
    pub fn new(rest: BinanceRest, step_size: HashMap<String, Decimal>) -> Self {
        Self {
            rest,
            step_size,
            lost: Mutex::new(HashMap::new()),
        }
    }

    /// The REST client this adapter trades through, which the account reads
    /// share, and the reads of [`BinanceRest`] (commission, key restrictions,
    /// the book, a symbol's rules) are made through, with its keys, connections
    /// and clock.
    pub fn rest(&self) -> &BinanceRest {
        &self.rest
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
        symbol: &str,
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
                symbol,
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
                symbol,
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

    /// Places `req` and reads what became of it: [`CexExecutor::execute`]
    /// without its errors' provenance.
    async fn place(&self, req: &OrderRequest) -> Result<CexFill> {
        let (side, quantity) = self.market_order(req)?;
        let client_order_id = new_client_order_id();
        let what = format!("{side} {quantity} {} ({client_order_id})", req.symbol);
        let placed = self
            .rest
            .place_market_order(&req.symbol, side, quantity, &client_order_id)
            .await;
        // When this request was signed, if its answer was lost: what a later
        // "no such order" is told conclusive against.
        let lost_at = match &placed {
            Err(ApiError::Lost { signed_at, .. }) => Some(*signed_at),
            _ => None,
        };
        let order = match settled(
            self.rest.client(),
            ORDER_PATH,
            &req.symbol,
            &client_order_id,
            &what,
            placed,
        )
        .await
        {
            Ok(order) => order,
            Err(err) => {
                if let Some(signed_at) = lost_at {
                    if err.downcast_ref::<OrderStateUnknown>().is_some() {
                        self.lost
                            .lock()
                            .unwrap()
                            .insert(client_order_id.clone(), signed_at);
                    }
                }
                return Err(err);
            }
        };
        self.settle(&req.symbol, &client_order_id, order).await
    }

    /// An order as the venue reports it, as a state: what it filled if it
    /// ended, `Open` if it has not.
    async fn state_of(
        &self,
        symbol: &str,
        client_order_id: &str,
        order: BinanceOrderResponse,
    ) -> Result<OrderState> {
        if !is_terminal(&order.status) {
            return Ok(OrderState::Open);
        }
        if order.executed_qty.is_zero() {
            return Ok(OrderState::Rejected {
                order_ref: order.order_id,
                status: order.status,
            });
        }
        let filled = order.status == "FILLED";
        let fill = self.settle(symbol, client_order_id, order).await?;
        Ok(if filled {
            OrderState::Filled(fill)
        } else {
            OrderState::PartlyFilled(fill)
        })
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
                let message = format!(
                    "the test of {side} {quantity} {} ({client_order_id}) failed: {err}",
                    req.symbol
                );
                err.because(message)
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
    /// Every `Err` carries `Provenance::Landed`: the provenance of the fill
    /// this order would have been, even when it was refused before it was sent.
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill> {
        self.place(req)
            .await
            .map_err(|err| with_provenance(err, Provenance::Landed))
    }

    fn label(&self) -> &'static str {
        "binance-live"
    }
}

#[async_trait]
impl CexOrders for BinanceLive {
    async fn order_state(&self, symbol: &str, client_order_id: &str) -> Result<OrderState> {
        // Taken before the query is signed and sent, so an answer counted as
        // given after `recvWindow` was certainly asked after it.
        let asked_at = Instant::now();
        let client = self.rest.client();
        match read_order::<BinanceOrderResponse>(client, ORDER_PATH, symbol, client_order_id).await
        {
            Ok(order) => {
                self.lost.lock().unwrap().remove(client_order_id);
                self.state_of(symbol, client_order_id, order)
                    .await
                    .with_context(|| format!("reading what order {client_order_id} filled"))
            }
            Err(err) if err.code() == Some(NO_SUCH_ORDER) => {
                let signed_at = self.lost.lock().unwrap().get(client_order_id).copied();
                match signed_at {
                    // The venue could still accept the request that placed it.
                    Some(signed_at) if asked_at <= client.recv_window_ends(signed_at) => {
                        Ok(OrderState::Open)
                    }
                    _ => {
                        self.lost.lock().unwrap().remove(client_order_id);
                        Ok(OrderState::NotFound)
                    }
                }
            }
            Err(err) => Err(anyhow::Error::from(err)
                .context(format!("reading order {client_order_id} on {symbol}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance::rest::BinanceConfig;
    use crate::cex::{CexTimings, CexTrade, CommissionRates, MakerTaker, OrderStateUnknown};
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

    /// A caller acts on `-2010` and `-2015` by reading the code, not by
    /// searching the text for it; the text is what it always was.
    #[tokio::test]
    async fn a_refused_order_is_a_venue_refusal_with_the_text_it_always_had() {
        use crate::cex::{provenance_of, refusal_of, VenueRefusal};

        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -2010, "msg": "Account has insufficient balance for requested action."
            })),
        )
        .await;

        let err = live(&server, decimal("0.01"))
            .execute(&request())
            .await
            .unwrap_err();

        assert_eq!(
            refusal_of(&err),
            Some(&VenueRefusal {
                status: 400,
                code: Some(-2010),
                msg: "Account has insufficient balance for requested action.".to_string(),
            })
        );
        let message = err.to_string();
        assert!(message.starts_with("BUY 10.03 SOLUSDT (vp-"), "{message}");
        assert!(
            message.ends_with(
                ") was not placed, nothing filled: refused (HTTP 400, code -2010): \
                 Account has insufficient balance for requested action."
            ),
            "{message}"
        );
        assert_eq!(provenance_of(&err), Some(Provenance::Landed));
        assert!(err.downcast_ref::<OrderStateUnknown>().is_none());
    }

    /// A refusal the venue gives with no body of its own (a gateway's) has no
    /// code, never a made-up one.
    #[tokio::test]
    async fn a_refusal_with_no_venue_code_has_none() {
        use crate::cex::{refusal_of, VenueRefusal};

        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(403).set_body_string("forbidden"),
        )
        .await;

        let err = live(&server, decimal("0.01"))
            .execute(&request())
            .await
            .unwrap_err();

        assert_eq!(
            refusal_of(&err),
            Some(&VenueRefusal {
                status: 403,
                code: None,
                msg: "forbidden".to_string(),
            })
        );
        assert!(
            err.to_string()
                .ends_with("nothing filled: refused (HTTP 403): forbidden"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_refused_order_test_is_a_venue_refusal_with_the_text_it_always_had() {
        use crate::cex::refusal_of;

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

        let refusal = refusal_of(&err).expect("the venue refused the test");
        assert_eq!((refusal.status, refusal.code), (400, Some(-1013)));
        let message = err.to_string();
        assert!(
            message.starts_with("the test of BUY 10.03 SOLUSDT (vp-"),
            "{message}"
        );
        assert!(
            message.ends_with(") failed: refused (HTTP 400, code -1013): Filter failure: NOTIONAL"),
            "{message}"
        );
    }

    /// A status query the venue refuses is an `Err` the caller reads the code
    /// from: `-2015` says the key or its address is wrong, not that the order
    /// is missing.
    #[tokio::test]
    async fn a_refused_status_query_is_a_venue_refusal() {
        use crate::cex::refusal_of;

        let server = venue().await;
        mount_status(
            &server,
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -2015, "msg": "Invalid API-key, IP, or permissions for action."
            })),
        )
        .await;

        let err = live(&server, decimal("0.01"))
            .order_state("SOLUSDT", "vp-1")
            .await
            .unwrap_err();

        assert_eq!(refusal_of(&err).unwrap().code, Some(-2015));
        assert_eq!(err.to_string(), "reading order vp-1 on SOLUSDT");
    }

    /// An order the venue may have filled is not a refusal, whatever the status
    /// query is answered with: the venue did act on, or may have acted on, the
    /// request, and `OrderStateUnknown` is what says so.
    #[tokio::test]
    async fn an_order_left_unknown_is_not_a_refusal() {
        use crate::cex::refusal_of;

        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(503).set_body_string("Unknown error"),
        )
        .await;
        mount_status(
            &server,
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -2015, "msg": "Invalid API-key, IP, or permissions for action."
            })),
        )
        .await;

        let err = live(&server, decimal("0.01"))
            .execute(&request())
            .await
            .unwrap_err();

        assert!(err.downcast_ref::<OrderStateUnknown>().is_some(), "{err:#}");
        assert!(refusal_of(&err).is_none(), "{err:#}");
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
        crate::testkit::contract::assert_fill_shape(&fill, "binance-live");

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

    /// Every way `execute` fails says `Landed`, so a caller records where the
    /// order went without asking the adapter: a refusal before anything is
    /// sent, the venue's refusal, an order that ended with nothing filled,
    /// and an order whose state is unknown. The messages are the ones the
    /// tests above read.
    #[tokio::test]
    async fn every_error_carries_the_landed_provenance() {
        use crate::cex::provenance_of;

        let server = venue().await;
        let err = live(&server, decimal("0.01"))
            .execute(&OrderRequest {
                reduce_only: true,
                ..request()
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reduce_only"), "{err}");
        assert_eq!(provenance_of(&err), Some(Provenance::Landed), "{err:#}");

        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -2010, "msg": "Account has insufficient balance for requested action."
            })),
        )
        .await;
        let err = live(&server, decimal("0.01"))
            .execute(&request())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("-2010"), "{err}");
        assert_eq!(provenance_of(&err), Some(Provenance::Landed), "{err:#}");

        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderId": 7, "status": "REJECTED", "executedQty": "0", "fills": []
            })),
        )
        .await;
        let err = live(&server, decimal("0.01"))
            .execute(&request())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("REJECTED"), "{err}");
        assert_eq!(provenance_of(&err), Some(Provenance::Landed), "{err:#}");

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
        let err = live(&server, decimal("0.01"))
            .execute(&request())
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<OrderStateUnknown>().is_some(), "{err:#}");
        assert_eq!(provenance_of(&err), Some(Provenance::Landed), "{err:#}");
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
                discount.asset.as_deref(),
                discount.rate,
                discount.enabled_for_account
            ),
            (Some("BNB"), decimal("0.25"), true)
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

    #[test]
    fn the_spot_testnets_own_answer_reads_with_no_discount_asset() {
        // Its answer to an order test, 3 October 2026: every rate zero and a
        // null discount asset, which the documentation does not show.
        let rates: CommissionRates = serde_json::from_str(
            r#"{"standardCommissionForOrder":{"maker":"0.00000000","taker":"0.00000000"},"specialCommissionForOrder":{"maker":"0.00000000","taker":"0.00000000"},"taxCommissionForOrder":{"maker":"0.00000000","taker":"0.00000000"},"discount":{"enabledForAccount":true,"enabledForSymbol":true,"discountAsset":null,"discount":"0.00000000"}}"#,
        )
        .unwrap();
        let discount = rates.discount.unwrap();
        assert_eq!((discount.asset, discount.rate), (None, Decimal::ZERO));
        assert_eq!(rates.standard.taker, Decimal::ZERO);
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

    // ---- The spot testnet (plan V3 of arb-searcher's execution blotter) ----
    //
    // Ignored: it places real orders on Binance's spot testnet. Run with
    //   cargo test --lib binance_spot_testnet -- --ignored --nocapture
    // and BINANCE_API_KEY / BINANCE_API_SECRET holding a spot *testnet* key.
    // It refuses any host but the testnet's, whatever BINANCE_BASE_URL says.
    // BINANCE_SPOT_TEST_SYMBOL picks the symbol (AEROUSDT by default); the
    // symbol's step and minimum notional are read from the testnet itself.

    /// The `LOT_SIZE` step and the `NOTIONAL` (or `MIN_NOTIONAL`) minimum of
    /// `symbol`, from `GET /api/v3/exchangeInfo`.
    async fn spot_rules(rest: &BinanceRest, symbol: &str) -> (Decimal, Decimal) {
        let info: serde_json::Value = rest
            .client()
            .public_get("/api/v3/exchangeInfo", &[("symbol", symbol.to_string())])
            .await
            .unwrap_or_else(|err| panic!("reading {symbol}'s rules: {err}"));
        let filters = info["symbols"][0]["filters"]
            .as_array()
            .expect("the symbol's filters");
        let filter = |kind: &str, key: &str| {
            filters
                .iter()
                .find(|f| f["filterType"] == kind)
                .and_then(|f| f[key].as_str())
                .map(decimal)
        };
        let step = filter("LOT_SIZE", "stepSize").expect("a LOT_SIZE step");
        let minimum = filter("NOTIONAL", "minNotional")
            .or_else(|| filter("MIN_NOTIONAL", "minNotional"))
            .expect("a minimum notional");
        (step, minimum)
    }

    async fn spot_price(rest: &BinanceRest, symbol: &str) -> Decimal {
        let ticker: serde_json::Value = rest
            .client()
            .public_get("/api/v3/ticker/price", &[("symbol", symbol.to_string())])
            .await
            .unwrap_or_else(|err| panic!("reading {symbol}'s price: {err}"));
        decimal(ticker["price"].as_str().expect("a price"))
    }

    /// The smallest whole number of steps worth at least `usd` at `price`.
    fn steps_worth(usd: Decimal, price: Decimal, step: Decimal) -> Decimal {
        (usd / price / step).ceil() * step
    }

    /// What every testnet fill must name, and that it traded `qty`.
    fn assert_landed(fill: &CexFill, qty: Decimal, side: &str) {
        crate::testkit::contract::assert_fill_shape(fill, "binance-spot-testnet");
        assert_eq!(
            fill.provenance,
            Provenance::Landed,
            "{side}: a testnet order is landed"
        );
        assert_eq!(
            fill.filled_qty, qty,
            "{side}: a market order this small fills in full"
        );
        assert!(
            fill.client_order_id
                .as_deref()
                .is_some_and(|id| id.starts_with("vp-")),
            "{side}: {:?}",
            fill.client_order_id
        );
        let now = local_now_ms() as i128;
        let at = fill.venue_time_ms.expect("the venue's transactTime") as i128;
        assert!(
            (now - at).abs() < 60_000,
            "{side}: transactTime {at} against local {now}"
        );
        assert!(!fill.trades.is_empty(), "{side}: its trades are listed");
        assert!(
            fill.trades.iter().all(|t| t.trade_id.is_some()),
            "{side}: every trade has its id"
        );
        assert!(
            !fill.commission_asset.is_empty(),
            "{side}: a commission asset"
        );
    }

    #[tokio::test]
    #[ignore = "places orders on Binance's spot testnet; needs a testnet key"]
    async fn binance_spot_testnet() {
        let config = BinanceConfig::from_env()
            .expect("BINANCE_API_KEY and BINANCE_API_SECRET must hold a spot testnet key");
        assert!(
            config.base_url.trim_end_matches('/') == "https://testnet.binance.vision",
            "refusing to place orders anywhere but the spot testnet: {}",
            config.base_url
        );
        let symbol =
            std::env::var("BINANCE_SPOT_TEST_SYMBOL").unwrap_or_else(|_| "AEROUSDT".to_string());
        let rest = BinanceRest::new(config);
        let (step, minimum) = spot_rules(&rest, &symbol).await;
        let price = spot_price(&rest, &symbol).await;
        let live = BinanceLive::new(rest, HashMap::from([(symbol.clone(), step)]));
        let order = |side, quantity| OrderRequest {
            symbol: symbol.clone(),
            side,
            quantity,
            quoted_price: price,
            reduce_only: false,
        };
        let qty = steps_worth(minimum + Decimal::ONE, price, step);
        eprintln!(
            "{symbol}: step {step}, minimum notional {minimum}, price {price}; trading {qty}"
        );

        // 1. The test endpoint: an order over the minimum is accepted with
        //    its rates, one under it is refused, and neither is placed.
        let check = live
            .test_order(&order(OrderSide::Buy, qty))
            .await
            .expect("the order test");
        assert_eq!(check.quantity, qty);
        eprintln!("order test: {:?}", check.rates);
        let dust = steps_worth(minimum / Decimal::from(5), price, step);
        let refused = live
            .test_order(&order(OrderSide::Buy, dust))
            .await
            .expect_err("an order under the minimum notional is refused");
        eprintln!("order test under the minimum ({dust}): {refused:#}");
        assert!(
            format!("{refused:#}").contains("-1013"),
            "a filter failure: {refused:#}"
        );

        // 2. A market buy, then a market sell of the same quantity.
        let bought = live
            .execute(&order(OrderSide::Buy, qty))
            .await
            .expect("the market buy");
        eprintln!("buy: {bought:?}");
        assert_landed(&bought, qty, "buy");
        let sold = live
            .execute(&order(OrderSide::Sell, qty))
            .await
            .expect("the market sell");
        eprintln!("sell: {sold:?}");
        assert_landed(&sold, qty, "sell");
        assert_ne!(bought.client_order_id, sold.client_order_id);
    }

    // TODO(R2): replace with the recorded body. Copied verbatim from the Spot API
    // documentation, "Query order (USER_DATA)", Response (a LIMIT order that is
    // still working; its `//` annotation is the documentation's):
    // https://github.com/binance/binance-spot-api-docs/blob/master/rest-api.md#query-order-user_data
    const DOCUMENTED_ORDER_NEW: &str = r#"{
    "symbol": "LTCBTC",
    "orderId": 1,
    "orderListId": -1, // This field will always have a value of -1 if not an order list.
    "clientOrderId": "myOrder1",
    "price": "0.1",
    "origQty": "1.0",
    "executedQty": "0.0",
    "cummulativeQuoteQty": "0.0",
    "status": "NEW",
    "timeInForce": "GTC",
    "type": "LIMIT",
    "side": "BUY",
    "stopPrice": "0.0",
    "icebergQty": "0.0",
    "time": 1499827319559,
    "updateTime": 1499827319559,
    "isWorking": true,
    "workingTime": 1499827319559,
    "origQuoteOrderQty": "0.000000",
    "selfTradePreventionMode": "NONE"
}"#;

    // TODO(R2): replace with the recorded body. Copied verbatim from the Spot API
    // documentation, "New order (TRADE)", Response - FULL (a MARKET order that filled
    // in five trades; its `//` annotation is the documentation's). The documentation
    // shows no status-query body for a filled order, and this is the same order
    // object: the status query answers it without `fills` and `transactTime`.
    // https://github.com/binance/binance-spot-api-docs/blob/master/rest-api.md#new-order-trade
    const DOCUMENTED_ORDER_FULL: &str = r#"{
    "symbol": "BTCUSDT",
    "orderId": 28,
    "orderListId": -1, // Unless it's part of an order list, value will be -1
    "clientOrderId": "6gCrw2kRUAF9CvJDGP16IP",
    "transactTime": 1507725176595,
    "price": "0.00000000",
    "origQty": "10.00000000",
    "executedQty": "10.00000000",
    "origQuoteOrderQty": "0.000000",
    "cummulativeQuoteQty": "10.00000000",
    "status": "FILLED",
    "timeInForce": "GTC",
    "type": "MARKET",
    "side": "SELL",
    "workingTime": 1507725176595,
    "selfTradePreventionMode": "NONE",
    "fills": [
        {
            "price": "4000.00000000",
            "qty": "1.00000000",
            "commission": "4.00000000",
            "commissionAsset": "USDT",
            "tradeId": 56
        },
        {
            "price": "3999.00000000",
            "qty": "5.00000000",
            "commission": "19.99500000",
            "commissionAsset": "USDT",
            "tradeId": 57
        },
        {
            "price": "3998.00000000",
            "qty": "2.00000000",
            "commission": "7.99600000",
            "commissionAsset": "USDT",
            "tradeId": 58
        },
        {
            "price": "3997.00000000",
            "qty": "1.00000000",
            "commission": "3.99700000",
            "commissionAsset": "USDT",
            "tradeId": 59
        },
        {
            "price": "3995.00000000",
            "qty": "1.00000000",
            "commission": "3.99500000",
            "commissionAsset": "USDT",
            "tradeId": 60
        }
    ]
}"#;

    // TODO(R2): replace with the recorded body. Copied verbatim from the Spot API
    // documentation, "Account trade list (USER_DATA)", Response:
    // https://github.com/binance/binance-spot-api-docs/blob/master/rest-api.md#account-trade-list-user_data
    const DOCUMENTED_MY_TRADES: &str = r#"[
    {
        "symbol": "BNBBTC",
        "id": 28457,
        "orderId": 100234,
        "orderListId": -1,
        "price": "4.00000100",
        "qty": "12.00000000",
        "quoteQty": "48.000012",
        "commission": "10.10000000",
        "commissionAsset": "BNB",
        "time": 1499865549590,
        "isBuyer": true,
        "isMaker": false,
        "isBestMatch": true
    }
]"#;

    /// The documentation's example bodies carry `//` annotations, which JSON
    /// does not: they are cut, and nothing else is.
    fn without_doc_comments(body: &str) -> String {
        body.lines()
            .map(|line| line.find("//").map_or(line, |at| &line[..at]))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn documented(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(without_doc_comments(body), "application/json")
    }

    fn documented_order(body: &str) -> Result<BinanceOrderResponse> {
        Ok(serde_json::from_str(&without_doc_comments(body))?)
    }

    /// A venue that answers the status query with `response`.
    async fn mount_status(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/api/v3/order"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    // TODO(R2): the body is the documented error payload's shape (errors.md, "Error
    // codes for Binance") with the message the documentation gives -2013
    // ("-2013 NO_SUCH_ORDER: Order does not exist."); no recorded one exists yet.
    fn no_such_order() -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_raw(
            r#"{
    "code": -2013,
    "msg": "Order does not exist."
}"#,
            "application/json",
        )
    }

    #[tokio::test]
    async fn an_order_the_venue_reports_working_is_open() -> Result<()> {
        let server = venue().await;
        mount_status(&server, documented(DOCUMENTED_ORDER_NEW)).await;

        let state = live(&server, decimal("0.01"))
            .order_state("LTCBTC", "myOrder1")
            .await?;

        assert!(matches!(state, OrderState::Open), "{state:?}");
        // The query asked for this order, by the id it was placed under.
        let requests = server.received_requests().await.unwrap();
        let query = requests
            .iter()
            .find(|r| r.url.path() == "/api/v3/order")
            .expect("the order was asked about");
        assert_eq!(query.method.as_str(), "GET");
        assert_eq!(
            param(query, "origClientOrderId").as_deref(),
            Some("myOrder1")
        );
        assert_eq!(param(query, "symbol").as_deref(), Some("LTCBTC"));
        Ok(())
    }

    /// A filled order is read as `execute` reads one: the fill, its price from its
    /// trades, one commission, each trade, the id it was found by.
    #[tokio::test]
    async fn a_filled_order_is_filled_with_its_fill_read_as_execute_reads_it() -> Result<()> {
        let server = venue().await;
        mount_status(&server, documented(DOCUMENTED_ORDER_FULL)).await;
        let adapter = live(&server, decimal("0.01"));

        let OrderState::Filled(fill) = adapter.order_state("BTCUSDT", "vp-lost-1").await? else {
            bail!("a FILLED order is Filled");
        };

        assert_eq!(fill.filled_qty, decimal("10"));
        // 4000 + 5 x 3999 + 2 x 3998 + 3997 + 3995 = 39983 over 10.
        assert_eq!(fill.filled_price, decimal("3998.3"));
        assert_eq!(fill.commission, decimal("39.983"));
        assert_eq!(fill.commission_asset, "USDT");
        assert_eq!(fill.provenance, Provenance::Landed);
        assert_eq!(fill.order_ref, Some(28));
        assert_eq!(fill.client_order_id.as_deref(), Some("vp-lost-1"));
        assert_eq!(fill.venue_time_ms, Some(1_507_725_176_595));
        assert_eq!(
            fill.trades.iter().map(|t| t.trade_id).collect::<Vec<_>>(),
            [56, 57, 58, 59, 60].map(Some)
        );
        crate::testkit::contract::assert_fill_shape(&fill, "binance-live");

        crate::testkit::contract::cex_orders_contract(
            &adapter,
            crate::testkit::contract::CexOrdersContractFixture {
                symbol: "BTCUSDT".to_string(),
                client_order_id: "vp-lost-1".to_string(),
            },
        )
        .await;
        Ok(())
    }

    /// An order the status query reports filled has no `fills`: its trade lines
    /// are read from `myTrades`, as `execute` reads them for an order it found
    /// by a status query. The order is the documented working order, read as
    /// filled in the one trade the documented `myTrades` lists.
    #[tokio::test]
    async fn a_filled_order_with_no_fills_reads_its_trade_lines_from_my_trades() -> Result<()> {
        let server = venue().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/myTrades"))
            .respond_with(documented(DOCUMENTED_MY_TRADES))
            .mount(&server)
            .await;
        let mut order = documented_order(DOCUMENTED_ORDER_NEW)?;
        order.order_id = 100_234;
        order.status = "FILLED".to_string();
        order.executed_qty = decimal("12");

        let state = live(&server, decimal("0.01"))
            .state_of("BNBBTC", "vp-1", order)
            .await?;

        let OrderState::Filled(fill) = state else {
            bail!("a FILLED order is Filled: {state:?}");
        };
        assert_eq!(fill.filled_qty, decimal("12"));
        assert_eq!(fill.filled_price, decimal("4.000001"));
        assert_eq!(fill.commission, decimal("10.1"));
        assert_eq!(fill.commission_asset, "BNB");
        assert_eq!(fill.trades.len(), 1);
        assert_eq!(fill.trades[0].trade_id, Some(28_457));
        // The status query's own time, since it carries no `transactTime`.
        assert_eq!(fill.venue_time_ms, Some(1_499_827_319_559));
        Ok(())
    }

    /// A market order that ran out of book ends `EXPIRED` with part filled: what
    /// `execute` returns as a partial fill. The order is the documented one,
    /// read as expired after its first two trades.
    #[tokio::test]
    async fn an_order_that_ended_with_part_filled_is_partly_filled() -> Result<()> {
        let server = venue().await;
        let mut order = documented_order(DOCUMENTED_ORDER_FULL)?;
        order.status = "EXPIRED".to_string();
        order.fills.truncate(2);
        order.executed_qty = decimal("6");

        let state = live(&server, decimal("0.01"))
            .state_of("BTCUSDT", "vp-1", order)
            .await?;

        let OrderState::PartlyFilled(fill) = state else {
            bail!("an EXPIRED order with part filled is PartlyFilled: {state:?}");
        };
        assert_eq!(fill.filled_qty, decimal("6"));
        // (4000 + 5 x 3999) / 6.
        assert_eq!(fill.filled_price.round_dp(6), decimal("3999.166667"));
        assert_eq!(fill.trades.len(), 2);
        Ok(())
    }

    /// An order that ended with nothing filled exposes nothing, whichever terminal
    /// status it ended in; one still working is open, whatever it has filled.
    #[tokio::test]
    async fn an_order_that_ended_with_nothing_filled_is_rejected() -> Result<()> {
        let server = venue().await;
        let adapter = live(&server, decimal("0.01"));
        for status in ["REJECTED", "EXPIRED", "CANCELED", "EXPIRED_IN_MATCH"] {
            let mut order = documented_order(DOCUMENTED_ORDER_FULL)?;
            order.status = status.to_string();
            order.executed_qty = Decimal::ZERO;
            order.fills.clear();

            match adapter.state_of("BTCUSDT", "vp-1", order).await? {
                OrderState::Rejected {
                    order_ref,
                    status: got,
                } => {
                    assert_eq!((order_ref, got.as_str()), (28, status));
                }
                other => bail!("{status} with nothing filled is Rejected: {other:?}"),
            }
        }

        let mut working = documented_order(DOCUMENTED_ORDER_FULL)?;
        working.status = "PARTIALLY_FILLED".to_string();
        working.fills.truncate(1);
        working.executed_qty = decimal("1");
        assert!(matches!(
            adapter.state_of("BTCUSDT", "vp-1", working).await?,
            OrderState::Open
        ));
        Ok(())
    }

    /// An order whose fill cannot be read is a failed read, not a state: the
    /// order filled and its commission is unknown, so nothing is returned.
    #[tokio::test]
    async fn a_fill_whose_trade_lines_cannot_be_read_is_an_error() -> Result<()> {
        let server = venue().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/myTrades"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let mut order = documented_order(DOCUMENTED_ORDER_NEW)?;
        order.status = "FILLED".to_string();
        order.executed_qty = decimal("12");

        let err = live(&server, decimal("0.01"))
            .state_of("BNBBTC", "vp-1", order)
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("could not be read"), "{err:#}");
        Ok(())
    }

    /// An id this adapter holds no lost request for was not placed by a request
    /// that may still be accepted: the venue's "no such order" is final.
    #[tokio::test]
    async fn an_id_with_no_lost_request_is_not_found_at_once() -> Result<()> {
        let server = venue().await;
        mount_status(&server, no_such_order()).await;

        let state = live(&server, decimal("0.01"))
            .order_state("SOLUSDT", "vp-never-placed")
            .await?;

        assert!(matches!(state, OrderState::NotFound), "{state:?}");
        Ok(())
    }

    /// For an order whose placing answer was lost, "no such order" says nothing
    /// until `recvWindow` and the clock's error bound have passed since the
    /// request was signed: until then the venue could still accept it, so the
    /// order is `Open`. After, nothing can, and it is `NotFound`, which is
    /// remembered no more.
    #[tokio::test]
    async fn not_found_is_conclusive_only_after_the_receive_window_and_the_clock_bound(
    ) -> Result<()> {
        let server = venue().await;
        mount_status(&server, no_such_order()).await;
        let adapter = live(&server, decimal("0.01"));
        let signed_at = Instant::now();
        adapter
            .lost
            .lock()
            .unwrap()
            .insert("vp-lost-1".to_string(), signed_at);

        let early = adapter.order_state("SOLUSDT", "vp-lost-1").await?;
        assert!(matches!(early, OrderState::Open), "{early:?}");
        assert!(adapter.lost.lock().unwrap().contains_key("vp-lost-1"));

        // The window (300 ms), the clock bound (one local round trip) and a margin.
        tokio::time::sleep(fast().recv_window + Duration::from_millis(150)).await;
        let late = adapter.order_state("SOLUSDT", "vp-lost-1").await?;
        assert!(matches!(late, OrderState::NotFound), "{late:?}");
        assert!(adapter.lost.lock().unwrap().is_empty());
        Ok(())
    }

    /// A read that fails is an error, never `NotFound`: not even for an order
    /// whose window has long passed.
    #[tokio::test]
    async fn a_failed_read_is_an_error_never_not_found() -> Result<()> {
        let server = venue().await;
        mount_status(
            &server,
            ResponseTemplate::new(503).set_body_string("Unknown error"),
        )
        .await;
        let adapter = live(&server, decimal("0.01"));
        adapter.lost.lock().unwrap().insert(
            "vp-lost-1".to_string(),
            Instant::now() - Duration::from_secs(60),
        );

        let err = adapter
            .order_state("SOLUSDT", "vp-lost-1")
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("vp-lost-1"), "{err:#}");
        assert!(adapter.lost.lock().unwrap().contains_key("vp-lost-1"));
        Ok(())
    }

    /// The whole road: an order whose placing answer is lost and whose status
    /// cannot be read ends `OrderStateUnknown`, and its id then resolves to what
    /// the venue did with it.
    #[tokio::test]
    async fn an_order_left_unknown_is_resolved_by_the_id_the_error_names() -> Result<()> {
        let server = venue().await;
        mount_placing(
            &server,
            ResponseTemplate::new(200)
                .set_body_json(filled_order("FILLED", "10.03"))
                .set_delay(Duration::from_millis(600)),
        )
        .await;
        let unreadable = Mock::given(method("GET"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(500))
            .mount_as_scoped(&server)
            .await;
        let adapter = live(&server, decimal("0.01"));

        let err = adapter.execute(&request()).await.unwrap_err();
        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .ok_or_else(|| anyhow!("an OrderStateUnknown: {err:#}"))?;
        assert_eq!(unknown.order_ref, None);
        assert!(adapter
            .lost
            .lock()
            .unwrap()
            .contains_key(&unknown.client_order_id));

        // The venue did take the order, and it filled.
        drop(unreadable);
        mount_status(&server, documented(DOCUMENTED_ORDER_FULL)).await;
        let state = adapter
            .order_state(&unknown.symbol, &unknown.client_order_id)
            .await?;

        let OrderState::Filled(fill) = state else {
            bail!("the order filled: {state:?}");
        };
        assert_eq!(
            fill.client_order_id.as_deref(),
            Some(unknown.client_order_id.as_str())
        );
        assert!(adapter.lost.lock().unwrap().is_empty());
        Ok(())
    }
}
