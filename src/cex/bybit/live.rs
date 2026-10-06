//! `BybitLive` — the second `CexLive` implementation, built alongside
//! Binance rather than after it. Pointing `BybitConfig::base_url` at
//! Bybit's testnet host (the default) makes this the "Simulated" mode for
//! the Bybit leg, per §3.
//!
//! **An `Err` means nothing filled, unless it is an `OrderStateUnknown`**
//! (`SPEC.md` §6). To keep to that:
//!
//! - Every order carries an `orderLinkId` generated for that call, so Bybit
//!   can be asked about it when the answer to the placing call is lost (a
//!   timeout, a dropped connection, an HTTP 5xx, an ambiguous `retCode`).
//! - Once Bybit has accepted an order, or may have, a status query that
//!   keeps failing, or an order that has not settled by `poll_timeout`, is
//!   `OrderStateUnknown`, never a plain error. So is an order Bybit never
//!   lists after a lost answer: unlike Binance's `-2013`, nothing in
//!   Bybit's documentation makes "not listed" conclusive (see below).
//! - A refusal (a non-zero `retCode`, or an HTTP 4xx) is a plain error
//!   carrying Bybit's code and message: nothing was placed.
//!
//! **Adapter conventions, flagged rather than guessed** — no sandbox
//! account exists yet to confirm any against a real response
//! (Blocker 3, `IMPLEMENTATION_PLAN.md`):
//! - Like `BinanceLive`, the `LOT_SIZE` step per symbol is supplied by the
//!   caller, not fetched from `GET /v5/instruments-info`.
//! - Bybit's own placement response never carries fill details, so this
//!   adapter always follows up with `GET /v5/order/realtime` (by `orderId`,
//!   or by `orderLinkId` after a lost answer), polling until the order
//!   reaches a terminal status. Whether that endpoint lists an order that
//!   has already closed depends on the account type, which is why an
//!   order it never lists is `OrderStateUnknown` rather than "never
//!   placed".
//! - The commission asset per symbol is also supplied by the caller
//!   (Bybit's order-status response reports a fee amount but not its
//!   currency); this is the one convention closest to a guess, since which
//!   asset a spot fee is actually charged in depends on account-level fee
//!   settings (and on the order's side) this adapter cannot see. An order
//!   for a symbol with none configured is refused before it is sent.

use crate::cex::bybit::rest::{BybitOrder, BybitRest, OrderKey, PlaceError};
use crate::cex::{
    new_client_order_id, CexExecutor, CexFill, OrderRequest, OrderSide, OrderStateUnknown,
};
use crate::Provenance;
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::Instant;

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

    /// Polls the order until it reaches a terminal status and reads it, or
    /// gives up with `OrderStateUnknown` at `poll_timeout`. `lost` is the
    /// reason the placing call's answer was lost, if it was.
    async fn settle(
        &self,
        req: &OrderRequest,
        order_link_id: &str,
        key: OrderKey<'_>,
        mut order_ref: Option<u64>,
        what: &str,
        lost: Option<anyhow::Error>,
    ) -> Result<CexFill> {
        let timings = self.rest.timings().clone();
        let deadline = Instant::now() + timings.poll_timeout;
        let mut problem = anyhow!("Bybit has not answered yet");
        loop {
            match self.rest.get_order(&req.symbol, key).await {
                Ok(Some(order)) => {
                    if let Ok(order_id) = order.order_id.parse() {
                        order_ref = Some(order_id);
                    }
                    if is_terminal(&order.order_status) {
                        return self.fill_from_order(req, order_link_id, order_ref, order);
                    }
                    problem = anyhow!(
                        "order {} was still {} when this adapter stopped waiting",
                        order.order_id,
                        order.order_status
                    );
                }
                Ok(None) => problem = anyhow!("Bybit did not list the order"),
                Err(err) => problem = err.context("the order-status query failed"),
            }
            if Instant::now() >= deadline {
                let why = match lost {
                    Some(lost) => format!(
                        "{what}: the answer to the placing call was lost ({lost:#}), and what \
                         became of the order could not be read: {problem:#}"
                    ),
                    None => format!(
                        "{what}: Bybit accepted the order, but what became of it could not be \
                         read: {problem:#}"
                    ),
                };
                return Err(OrderStateUnknown {
                    symbol: req.symbol.clone(),
                    client_order_id: order_link_id.to_string(),
                    order_ref,
                }
                .because(why));
            }
            tokio::time::sleep(timings.poll_interval).await;
        }
    }

    fn fill_from_order(
        &self,
        req: &OrderRequest,
        order_link_id: &str,
        order_ref: Option<u64>,
        order: BybitOrder,
    ) -> Result<CexFill> {
        if order.cum_exec_qty.is_zero() {
            bail!(
                "order {} ended in status {} with no fill",
                order.order_id,
                order.order_status
            );
        }
        let unknown = |why: String| {
            OrderStateUnknown {
                symbol: req.symbol.clone(),
                client_order_id: order_link_id.to_string(),
                order_ref,
            }
            .because(why)
        };
        let filled_price: Decimal = order.avg_price.parse().map_err(|_| {
            unknown(format!(
                "order {} filled {} but its avgPrice {:?} could not be read",
                order.order_id, order.cum_exec_qty, order.avg_price
            ))
        })?;
        let commission_asset = self
            .commission_asset
            .get(&req.symbol)
            .cloned()
            .ok_or_else(|| unknown(format!("no commission asset for {}", req.symbol)))?;

        Ok(CexFill {
            filled_qty: order.cum_exec_qty,
            filled_price,
            commission: order.cum_exec_fee,
            commission_asset,
            provenance: Provenance::Landed,
            order_ref,
            client_order_id: Some(order_link_id.to_string()),
            venue_time_ms: order.updated_time.as_deref().and_then(|ms| ms.parse().ok()),
            // This adapter reads the order, not its executions.
            trades: Vec::new(),
        })
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
        if !self.commission_asset.contains_key(&req.symbol) {
            bail!(
                "no commission asset configured for symbol {} — refusing to send an order whose \
                 fill could not be reported",
                req.symbol
            );
        }

        let order_link_id = new_client_order_id();
        let what = format!("{side} {quantity} {} ({order_link_id})", req.symbol);
        match self
            .rest
            .place_market_order(&req.symbol, side, quantity, &order_link_id)
            .await
        {
            Ok(order_id) => {
                self.settle(
                    req,
                    &order_link_id,
                    OrderKey::Id(&order_id),
                    order_id.parse().ok(),
                    &what,
                    None,
                )
                .await
            }
            Err(PlaceError::Lost(lost)) => {
                self.settle(
                    req,
                    &order_link_id,
                    OrderKey::LinkId(&order_link_id),
                    None,
                    &what,
                    Some(lost),
                )
                .await
            }
            Err(PlaceError::NotSent(err) | PlaceError::Refused(err)) => {
                Err(anyhow!("{what} was not placed, nothing filled: {err:#}"))
            }
        }
    }

    fn label(&self) -> &'static str {
        "bybit-live"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::bybit::rest::BybitConfig;
    use crate::cex::CexTimings;
    use std::str::FromStr;
    use std::time::Duration;
    use wiremock::matchers::{method, path, query_param};
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

    fn live(server: &MockServer) -> BybitLive {
        let rest = BybitRest::with_timings(
            BybitConfig {
                base_url: server.uri(),
                api_key: "test-key".to_string(),
                api_secret: "test-secret".to_string(),
            },
            CexTimings {
                request_timeout: Duration::from_millis(200),
                poll_interval: Duration::from_millis(10),
                poll_timeout: Duration::from_millis(200),
                ..CexTimings::default()
            },
        );
        BybitLive::new(
            rest,
            HashMap::from([("SOLUSDT".to_string(), decimal("0.01"))]),
            HashMap::from([("SOLUSDT".to_string(), "USDT".to_string())]),
        )
    }

    fn created(order_id: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"orderId": order_id, "orderLinkId": ""},
            "retExtInfo": {}, "time": 1
        }))
    }

    fn listed(status: &str, avg_price: &str, qty: &str, fee: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [{
                "orderId": "9001",
                "orderStatus": status,
                "avgPrice": avg_price,
                "cumExecQty": qty,
                "cumExecFee": fee,
                "updatedTime": "1684738540561"
            }]},
            "retExtInfo": {}, "time": 2
        }))
    }

    async fn mount(
        server: &MockServer,
        http_method: &str,
        route: &str,
        response: ResponseTemplate,
    ) {
        Mock::given(method(http_method))
            .and(path(route))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn sent_link_id(requests: &[Request]) -> String {
        let create = requests
            .iter()
            .find(|r| r.url.path() == "/v5/order/create")
            .unwrap();
        create.body_json::<serde_json::Value>().unwrap()["orderLinkId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn a_filled_order_becomes_a_landed_fill_after_polling_status() {
        let server = MockServer::start().await;
        mount(&server, "POST", "/v5/order/create", created("9001")).await;
        mount(
            &server,
            "GET",
            "/v5/order/realtime",
            listed("Filled", "150.1", "10.03", "0.02"),
        )
        .await;

        let adapter = live(&server);
        let fill = adapter.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("10.03"));
        assert_eq!(fill.filled_price, decimal("150.1"));
        assert_eq!(fill.commission, decimal("0.02"));
        assert_eq!(fill.commission_asset, "USDT");
        assert_eq!(fill.provenance, Provenance::Landed);
        assert_eq!(fill.order_ref, Some(9001));

        let link_id = sent_link_id(&server.received_requests().await.unwrap());
        assert!(
            link_id.starts_with("vp-") && link_id.len() <= 36,
            "{link_id}"
        );
        assert_eq!(fill.client_order_id, Some(link_id));
        assert_eq!(fill.venue_time_ms, Some(1_684_738_540_561));
        assert!(fill.trades.is_empty(), "this adapter lists no trades");
        crate::testkit::contract::assert_fill_shape(&fill, adapter.label());
    }

    #[tokio::test]
    async fn a_rejected_order_is_an_error() {
        let server = MockServer::start().await;
        mount(&server, "POST", "/v5/order/create", created("9002")).await;
        mount(
            &server,
            "GET",
            "/v5/order/realtime",
            listed("Rejected", "0", "0", "0"),
        )
        .await;

        let adapter = live(&server);
        let err = adapter.execute(&request()).await.unwrap_err();
        assert!(err.to_string().contains("Rejected"));
        assert!(err.downcast_ref::<OrderStateUnknown>().is_none());
    }

    #[tokio::test]
    async fn a_refusal_is_a_plain_error_carrying_the_venues_code() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/v5/order/create",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 170131, "retMsg": "Insufficient balance.",
                "result": {}, "retExtInfo": {}, "time": 1
            })),
        )
        .await;

        let adapter = live(&server);
        let err = adapter.execute(&request()).await.unwrap_err();

        assert!(err.downcast_ref::<OrderStateUnknown>().is_none());
        let message = err.to_string();
        assert!(message.contains("170131"), "{message}");
        assert!(message.contains("Insufficient balance."), "{message}");
    }

    #[tokio::test]
    async fn a_lost_answer_is_recovered_by_the_order_link_id() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/v5/order/create",
            created("9001").set_delay(Duration::from_millis(600)),
        )
        .await;
        mount(
            &server,
            "GET",
            "/v5/order/realtime",
            listed("Filled", "150.1", "10.03", "0.02"),
        )
        .await;

        let adapter = live(&server);
        let fill = adapter.execute(&request()).await.unwrap();

        assert_eq!(fill.filled_qty, decimal("10.03"));
        assert_eq!(fill.order_ref, Some(9001));
        let requests = server.received_requests().await.unwrap();
        let link_id = sent_link_id(&requests);
        let query = requests
            .iter()
            .find(|r| r.url.path() == "/v5/order/realtime")
            .unwrap();
        assert!(query
            .url
            .query_pairs()
            .any(|(k, v)| k == "orderLinkId" && v == link_id));
    }

    #[tokio::test]
    async fn a_5xx_answer_counts_as_lost_not_refused() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/v5/order/create",
            ResponseTemplate::new(502),
        )
        .await;
        mount(
            &server,
            "GET",
            "/v5/order/realtime",
            listed("Filled", "150.1", "10.03", "0.02"),
        )
        .await;

        let fill = live(&server).execute(&request()).await.unwrap();
        assert_eq!(fill.order_ref, Some(9001));
    }

    #[tokio::test]
    async fn a_lost_answer_and_an_order_never_listed_is_order_state_unknown() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/v5/order/create",
            created("9001").set_delay(Duration::from_millis(600)),
        )
        .await;
        mount(
            &server,
            "GET",
            "/v5/order/realtime",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 0, "retMsg": "OK", "result": {"list": []}, "retExtInfo": {}, "time": 2
            })),
        )
        .await;

        let err = live(&server).execute(&request()).await.unwrap_err();

        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown");
        assert_eq!(unknown.order_ref, None);
        assert!(unknown.client_order_id.starts_with("vp-"));
    }

    #[tokio::test]
    async fn a_failed_poll_after_acceptance_is_order_state_unknown() {
        let server = MockServer::start().await;
        mount(&server, "POST", "/v5/order/create", created("9001")).await;
        mount(
            &server,
            "GET",
            "/v5/order/realtime",
            ResponseTemplate::new(500),
        )
        .await;

        let err = live(&server).execute(&request()).await.unwrap_err();

        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown, never a plain error");
        assert_eq!(unknown.order_ref, Some(9001));
        assert_eq!(unknown.symbol, "SOLUSDT");
    }

    #[tokio::test]
    async fn an_order_that_never_settles_is_order_state_unknown() {
        let server = MockServer::start().await;
        mount(&server, "POST", "/v5/order/create", created("9001")).await;
        Mock::given(method("GET"))
            .and(path("/v5/order/realtime"))
            .and(query_param("orderId", "9001"))
            .respond_with(listed("New", "0", "0", "0"))
            .mount(&server)
            .await;

        let err = live(&server).execute(&request()).await.unwrap_err();

        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown");
        assert_eq!(unknown.order_ref, Some(9001));
        assert!(format!("{err:#}").contains("still New"), "{err:#}");
    }

    #[tokio::test]
    async fn an_unreadable_average_price_is_order_state_unknown() {
        let server = MockServer::start().await;
        mount(&server, "POST", "/v5/order/create", created("9001")).await;
        mount(
            &server,
            "GET",
            "/v5/order/realtime",
            listed("Filled", "", "10.03", "0.02"),
        )
        .await;

        let err = live(&server).execute(&request()).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<OrderStateUnknown>().unwrap().order_ref,
            Some(9001)
        );
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
