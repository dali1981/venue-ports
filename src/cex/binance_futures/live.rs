//! `BinanceFuturesLive`: a `CexExecutor` for Binance USDⓈ-M perpetuals
//! (`SPEC.md` §6, `specs/V2-binance-usdm-futures.md`). Pointed at the
//! testnet host (the default), it is this leg's Simulated mode (§3).
//!
//! [`BinanceFuturesLive::connect`] checks the account once and refuses to
//! return an adapter for one it cannot trade correctly (see
//! `BinanceFuturesRest::refuse_unsupported_account` and
//! `filters::symbol_filters`). Margin type and leverage are read by
//! `BinanceFuturesAccount` (§6b), never set here.
//!
//! `execute` takes five steps:
//!
//! 1. **Validate before sending.** Only a symbol given to `connect`;
//!    quantity rounded down to `MARKET_LOT_SIZE`, refused outside
//!    `minQty`/`maxQty`, and refused below `MIN_NOTIONAL` at the quoted
//!    price unless reduce-only.
//! 2. **Place** `POST /fapi/v1/order`: `type=MARKET`, `reduceOnly`,
//!    `newOrderRespType=RESULT` and a `newClientOrderId` generated for this
//!    call.
//! 3. **Read the result.** `FILLED`, or `EXPIRED`/`CANCELED` with a
//!    non-zero `executedQty` (a partial fill, never an error), is read as
//!    `filled_qty = executedQty`, `filled_price = avgPrice`. `NEW` or
//!    `PARTIALLY_FILLED` is polled at `GET /fapi/v1/order?origClientOrderId=`
//!    until it settles. An error code (`-2022` reduce-only rejected,
//!    `-4164` notional, `-2019` margin) is a plain error carrying the code
//!    and message: nothing filled.
//! 4. **Commission.** The `RESULT` answer has none, so it is the sum of
//!    `commission` over `GET /fapi/v1/userTrades?symbol=&orderId=`, in
//!    exactly one `commissionAsset`, read once the lines add up to
//!    `executedQty`. They can lag the fill, so they are read again for up to
//!    `trades_timeout` (2 s by default); after that the order is
//!    `OrderStateUnknown`. A fill is never returned with a guessed
//!    commission. The commission's sign is the venue's own (positive is
//!    paid, as on spot); Binance's documented example shows a negative
//!    value, so the testnet run should confirm it.
//! 5. **Lost answer.** A timeout, a dropped connection or an HTTP 5xx is
//!    followed by `GET /fapi/v1/order?origClientOrderId=`. Found: step 3.
//!    `-2013` once `recvWindow` has passed: never accepted, a plain error
//!    (the caller may retry). The query failing: `OrderStateUnknown` with no
//!    `order_ref`.
//!
//! `CexFill.order_ref` is the venue's `orderId`, and `provenance` is always
//! `Landed`, on the testnet too.
//!
//! The testnet runs at the bottom of this file are gated on
//! `BINANCE_FUTURES_API_KEY`/`BINANCE_FUTURES_API_SECRET`; the ones that
//! place many orders, the `SPEC.md` §9.2 run among them, also need
//! `BINANCE_FUTURES_RUN_ACCEPTANCE=1`.

use crate::cex::binance::order::{
    settled, single_commission, trade_lines, unreadable_fill, VenueOrder,
};
use crate::cex::binance_futures::filters::{symbol_filters, ExchangeInfo, SymbolFilters};
use crate::cex::binance_futures::rest::{BinanceFuturesConfig, BinanceFuturesRest};
use crate::cex::{new_client_order_id, CexExecutor, CexFill, OrderRequest, OrderSide};
use crate::Provenance;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::Method;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

const ORDER_PATH: &str = "/fapi/v1/order";
const USER_TRADES_PATH: &str = "/fapi/v1/userTrades";
const EXCHANGE_INFO_PATH: &str = "/fapi/v1/exchangeInfo";

/// The subset of a futures order this crate reads, as `POST /fapi/v1/order`
/// (with `newOrderRespType=RESULT`) and `GET /fapi/v1/order` both report it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FuturesOrder {
    #[serde(rename = "orderId")]
    pub(crate) order_id: u64,
    pub(crate) status: String,
    #[serde(rename = "executedQty")]
    pub(crate) executed_qty: Decimal,
    #[serde(rename = "avgPrice")]
    pub(crate) avg_price: Decimal,
}

impl VenueOrder for FuturesOrder {
    fn order_id(&self) -> u64 {
        self.order_id
    }

    fn status(&self) -> &str {
        &self.status
    }
}

pub struct BinanceFuturesLive {
    rest: Arc<BinanceFuturesRest>,
    /// The symbols given to `connect`, and what an order for each must
    /// satisfy.
    filters: HashMap<String, SymbolFilters>,
}

impl BinanceFuturesLive {
    /// Connects, then refuses to return an adapter for an account it cannot
    /// trade correctly. `symbols` are the only symbols `execute` will accept.
    pub async fn connect(config: BinanceFuturesConfig, symbols: &[&str]) -> Result<Self> {
        Self::connect_with(Arc::new(BinanceFuturesRest::new(config)), symbols).await
    }

    /// As [`Self::connect`], over a client the caller built: one with other
    /// timings, or one to share with a `BinanceFuturesAccount`.
    pub async fn connect_with(rest: Arc<BinanceFuturesRest>, symbols: &[&str]) -> Result<Self> {
        rest.refuse_unsupported_account(true)
            .await
            .with_context(|| format!("refusing to trade the account at {}", rest.base_url()))?;
        let info: ExchangeInfo = rest
            .client()
            .public_get(EXCHANGE_INFO_PATH, &[])
            .await
            .context("reading exchangeInfo")?;
        let filters = symbol_filters(&info, symbols)
            .with_context(|| format!("refusing to trade at {}", rest.base_url()))?;
        Ok(Self { rest, filters })
    }

    /// The client this adapter signs with, to share with a
    /// `BinanceFuturesAccount`.
    pub fn rest(&self) -> &Arc<BinanceFuturesRest> {
        &self.rest
    }

    /// Reads an order the venue reports in a terminal state into a fill.
    async fn fill(
        &self,
        req: &OrderRequest,
        client_order_id: &str,
        order: FuturesOrder,
    ) -> Result<CexFill> {
        if order.executed_qty.is_zero() {
            bail!(
                "order {} ({client_order_id}) ended in status {} with nothing filled",
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
        if order.avg_price <= Decimal::ZERO {
            return Err(unknown(anyhow!("its avgPrice reads {}", order.avg_price)));
        }
        let lines = trade_lines(
            self.rest.client(),
            USER_TRADES_PATH,
            &req.symbol,
            order.order_id,
            order.executed_qty,
        )
        .await
        .map_err(unknown)?;
        let (commission, commission_asset) = single_commission(&lines).map_err(unknown)?;

        Ok(CexFill {
            filled_qty: order.executed_qty,
            filled_price: order.avg_price,
            commission,
            commission_asset,
            provenance: Provenance::Landed,
            order_ref: Some(order.order_id),
        })
    }
}

#[async_trait]
impl CexExecutor for BinanceFuturesLive {
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill> {
        let filters = self.filters.get(&req.symbol).ok_or_else(|| {
            anyhow!(
                "{} was not among the symbols given to connect; refusing to send it",
                req.symbol
            )
        })?;
        let quantity =
            filters.validate(&req.symbol, req.quantity, req.quoted_price, req.reduce_only)?;
        let side = match req.side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };

        let client_order_id = new_client_order_id();
        let what = format!(
            "{side} {quantity} {}{} ({client_order_id})",
            req.symbol,
            if req.reduce_only { " reduce-only" } else { "" }
        );
        let params = [
            ("symbol", req.symbol.clone()),
            ("side", side.to_string()),
            ("type", "MARKET".to_string()),
            ("quantity", quantity.to_string()),
            ("reduceOnly", req.reduce_only.to_string()),
            ("newClientOrderId", client_order_id.clone()),
            ("newOrderRespType", "RESULT".to_string()),
        ];
        let client = self.rest.client();
        let placed = client
            .signed::<FuturesOrder>(Method::POST, ORDER_PATH, &params)
            .await;
        let order = settled(
            client,
            ORDER_PATH,
            &req.symbol,
            &client_order_id,
            &what,
            placed,
        )
        .await?;
        self.fill(req, &client_order_id, order).await
    }

    fn label(&self) -> &'static str {
        "binance-futures-live"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance::sign::sign;
    use crate::cex::binance_futures::filters::tests::exchange_info_json;
    use crate::cex::{CexTimings, OrderStateUnknown};
    use crate::testkit::contract::{cex_executor_contract, CexContractFixture};
    use std::str::FromStr;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const SECRET: &str = "test-secret";

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn fast() -> CexTimings {
        CexTimings {
            request_timeout: Duration::from_millis(200),
            recv_window: Duration::from_millis(300),
            clock_refresh: Duration::from_secs(600),
            poll_interval: Duration::from_millis(10),
            poll_timeout: Duration::from_millis(300),
            trades_timeout: Duration::from_millis(150),
        }
    }

    fn rest(server: &MockServer, timings: CexTimings) -> Arc<BinanceFuturesRest> {
        Arc::new(BinanceFuturesRest::with_timings(
            BinanceFuturesConfig {
                base_url: server.uri(),
                api_key: "test-key".to_string(),
                api_secret: SECRET.to_string(),
            },
            timings,
        ))
    }

    #[derive(Default)]
    struct Settings {
        hedge_mode: bool,
        multi_assets: bool,
        fee_burn: bool,
    }

    async fn get(server: &MockServer, route: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// A venue with an account `connect` accepts, unless `settings` say
    /// otherwise, listing the symbols of `exchange_info_json`.
    async fn venue_with(settings: Settings) -> MockServer {
        let server = MockServer::start().await;
        get(
            &server,
            "/fapi/v1/time",
            serde_json::json!({ "serverTime": local_now_ms() }),
        )
        .await;
        get(
            &server,
            "/fapi/v1/positionSide/dual",
            serde_json::json!({ "dualSidePosition": settings.hedge_mode }),
        )
        .await;
        get(
            &server,
            "/fapi/v1/multiAssetsMargin",
            serde_json::json!({ "multiAssetsMargin": settings.multi_assets }),
        )
        .await;
        get(
            &server,
            "/fapi/v1/feeBurn",
            serde_json::json!({ "feeBurn": settings.fee_burn }),
        )
        .await;
        get(&server, "/fapi/v1/exchangeInfo", exchange_info_json()).await;
        server
    }

    async fn venue() -> MockServer {
        venue_with(Settings::default()).await
    }

    async fn connect(server: &MockServer) -> BinanceFuturesLive {
        BinanceFuturesLive::connect_with(rest(server, fast()), &["BTCUSDT"])
            .await
            .unwrap()
    }

    async fn connect_err(rest: Arc<BinanceFuturesRest>, symbols: &[&str]) -> String {
        match BinanceFuturesLive::connect_with(rest, symbols).await {
            Ok(_) => panic!("connect accepted an account it should refuse"),
            Err(err) => format!("{err:#}"),
        }
    }

    fn order(status: &str, executed: &str, avg_price: &str) -> serde_json::Value {
        serde_json::json!({
            "orderId": 1001,
            "clientOrderId": "echoed",
            "symbol": "BTCUSDT",
            "status": status,
            "executedQty": executed,
            "cumQty": executed,
            "cumQuote": "0",
            "avgPrice": avg_price,
            "origQty": "0.006",
            "price": "0",
            "reduceOnly": false,
            "side": "BUY",
            "positionSide": "BOTH",
            "type": "MARKET",
            "origType": "MARKET",
            "timeInForce": "GTC",
            "updateTime": 1_700_000_000_000i64
        })
    }

    /// `userTrades` rows for order 1001, one per `(qty, commission, asset)`.
    fn trades(lines: &[(&str, &str, &str)]) -> serde_json::Value {
        serde_json::Value::Array(
            lines
                .iter()
                .enumerate()
                .map(|(id, (qty, commission, asset))| {
                    serde_json::json!({
                        "symbol": "BTCUSDT",
                        "id": id,
                        "orderId": 1001,
                        "side": "BUY",
                        "price": "60000",
                        "qty": qty,
                        "quoteQty": (d("60000") * d(qty)).to_string(),
                        "realizedPnl": "0",
                        "commission": commission,
                        "commissionAsset": asset,
                        "time": 1_700_000_000_000i64,
                        "positionSide": "BOTH",
                        "buyer": true,
                        "maker": false
                    })
                })
                .collect(),
        )
    }

    fn ok(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(body)
    }

    fn venue_error(code: i64, msg: &str) -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(serde_json::json!({ "code": code, "msg": msg }))
    }

    async fn on(server: &MockServer, http_method: &str, route: &str, response: ResponseTemplate) {
        Mock::given(method(http_method))
            .and(path(route))
            .respond_with(response)
            .mount(server)
            .await;
    }

    /// A venue that fills a 0.006 BTCUSDT buy at 60 000 for 0.144 USDT.
    async fn filling_venue() -> MockServer {
        let server = venue().await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")),
        )
        .await;
        on(
            &server,
            "GET",
            USER_TRADES_PATH,
            ok(trades(&[
                ("0.004", "0.096", "USDT"),
                ("0.002", "0.048", "USDT"),
            ])),
        )
        .await;
        server
    }

    fn buy(quantity: &str) -> OrderRequest {
        OrderRequest {
            symbol: "BTCUSDT".to_string(),
            side: OrderSide::Buy,
            quantity: d(quantity),
            quoted_price: d("60000"),
            reduce_only: false,
        }
    }

    fn param(request: &Request, key: &str) -> Option<String> {
        request
            .url
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }

    async fn requests_to(server: &MockServer, http_method: &str, route: &str) -> Vec<Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == http_method && r.url.path() == route)
            .collect()
    }

    #[tokio::test]
    async fn signs_every_private_call_with_the_signature_last() {
        let server = filling_venue().await;
        let live = connect(&server).await;
        live.execute(&buy("0.0079")).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let signed: Vec<&Request> = requests
            .iter()
            .filter(|r| r.headers.contains_key("X-MBX-APIKEY"))
            .collect();
        // positionSide/dual, multiAssetsMargin, feeBurn, order, userTrades.
        assert_eq!(signed.len(), 5);
        for request in &signed {
            let query = request.url.query().unwrap();
            let (unsigned, signature) = query.rsplit_once("&signature=").unwrap();
            assert_eq!(signature, sign(SECRET, unsigned), "{}", request.url);
            assert_eq!(request.headers.get("X-MBX-APIKEY").unwrap(), "test-key");
        }
        // Public calls carry neither key nor signature.
        for request in requests
            .iter()
            .filter(|r| r.url.path() == "/fapi/v1/time" || r.url.path() == "/fapi/v1/exchangeInfo")
        {
            assert!(!request.headers.contains_key("X-MBX-APIKEY"));
            assert!(param(request, "signature").is_none());
        }

        let placed = signed.iter().find(|r| r.method.as_str() == "POST").unwrap();
        let keys: Vec<String> = placed
            .url
            .query_pairs()
            .map(|(k, _)| k.into_owned())
            .collect();
        assert_eq!(
            keys,
            [
                "symbol",
                "side",
                "type",
                "quantity",
                "reduceOnly",
                "newClientOrderId",
                "newOrderRespType",
                "recvWindow",
                "timestamp",
                "signature"
            ]
        );
        assert_eq!(param(placed, "type").unwrap(), "MARKET");
        assert_eq!(param(placed, "reduceOnly").unwrap(), "false");
        assert_eq!(param(placed, "newOrderRespType").unwrap(), "RESULT");
        assert!(param(placed, "newClientOrderId")
            .unwrap()
            .starts_with("vp-"));
    }

    #[tokio::test]
    async fn connect_refuses_an_account_it_cannot_trade_correctly() {
        for (settings, why) in [
            (
                Settings {
                    hedge_mode: true,
                    ..Settings::default()
                },
                "hedge mode",
            ),
            (
                Settings {
                    multi_assets: true,
                    ..Settings::default()
                },
                "multi-assets margin",
            ),
            (
                Settings {
                    fee_burn: true,
                    ..Settings::default()
                },
                "pays fees in BNB",
            ),
        ] {
            let server = venue_with(settings).await;
            let err = connect_err(rest(&server, fast()), &["BTCUSDT"]).await;
            assert!(err.contains(why), "expected {why:?} in {err}");
        }
    }

    #[tokio::test]
    async fn connect_refuses_a_symbol_it_cannot_trade() {
        let server = venue().await;
        for (symbol, why) in [
            ("DOGEUSDT", "not listed"),
            ("OLDUSDT", "not trading"),
            ("ETHUSDT_260327", "not a perpetual"),
        ] {
            let err = connect_err(rest(&server, fast()), &["BTCUSDT", symbol]).await;
            assert!(err.contains(why), "{symbol}: expected {why:?} in {err}");
        }
    }

    #[tokio::test]
    async fn connect_refuses_a_clock_too_slow_for_recv_window() {
        let slow = MockServer::start().await;
        Mock::given(path("/fapi/v1/time"))
            .respond_with(
                ok(serde_json::json!({ "serverTime": local_now_ms() }))
                    .set_delay(Duration::from_millis(250)),
            )
            .mount(&slow)
            .await;
        let timings = CexTimings {
            request_timeout: Duration::from_secs(2),
            recv_window: Duration::from_millis(100),
            ..fast()
        };
        let err = connect_err(rest(&slow, timings), &["BTCUSDT"]).await;
        assert!(err.contains("does not fit inside recvWindow"), "{err}");
    }

    #[tokio::test]
    async fn rounds_down_to_the_market_step_before_sending() {
        let server = filling_venue().await;
        let live = connect(&server).await;
        live.execute(&buy("0.0079")).await.unwrap();

        let placed = requests_to(&server, "POST", ORDER_PATH).await;
        assert_eq!(param(&placed[0], "quantity").unwrap(), "0.006");
    }

    #[tokio::test]
    async fn refuses_below_the_minimum_quantity_or_notional_without_sending() {
        let server = venue().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let live = connect(&server).await;

        let err = live.execute(&buy("0.0019")).await.unwrap_err();
        assert!(err.to_string().contains("below the minimum 0.002"), "{err}");

        let cheap = OrderRequest {
            quantity: d("0.002"),
            quoted_price: d("30000"),
            ..buy("0")
        };
        let err = live.execute(&cheap).await.unwrap_err();
        assert!(err.to_string().contains("notional"), "{err}");

        let err = live
            .execute(&OrderRequest {
                symbol: "ETHUSDT".to_string(),
                ..buy("1")
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not among the symbols"), "{err}");
        // `expect(0)` is verified when `server` drops.
    }

    #[tokio::test]
    async fn a_reduce_only_order_below_the_notional_minimum_is_still_sent() {
        let server = filling_venue().await;
        let live = connect(&server).await;
        let close = OrderRequest {
            side: OrderSide::Sell,
            quantity: d("0.002"),
            quoted_price: d("30000"),
            reduce_only: true,
            ..buy("0")
        };
        live.execute(&close).await.unwrap();

        let placed = requests_to(&server, "POST", ORDER_PATH).await;
        assert_eq!(param(&placed[0], "reduceOnly").unwrap(), "true");
        assert_eq!(param(&placed[0], "side").unwrap(), "SELL");
    }

    #[tokio::test]
    async fn a_filled_order_reads_its_commission_from_user_trades() {
        let server = filling_venue().await;
        let live = connect(&server).await;
        let fill = live.execute(&buy("0.006")).await.unwrap();

        assert_eq!(fill.filled_qty, d("0.006"));
        assert_eq!(fill.filled_price, d("60000"));
        assert_eq!(fill.commission, d("0.144"));
        assert_eq!(fill.commission_asset, "USDT");
        assert_eq!(fill.provenance, Provenance::Landed);
        assert_eq!(fill.order_ref, Some(1001));

        let read = requests_to(&server, "GET", USER_TRADES_PATH).await;
        assert_eq!(param(&read[0], "orderId").unwrap(), "1001");
        assert_eq!(param(&read[0], "symbol").unwrap(), "BTCUSDT");
    }

    #[tokio::test]
    async fn a_market_order_that_expired_part_filled_is_a_partial_fill() {
        let server = venue().await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("EXPIRED", "0.004", "60010")),
        )
        .await;
        on(
            &server,
            "GET",
            USER_TRADES_PATH,
            ok(trades(&[("0.004", "0.096", "USDT")])),
        )
        .await;
        let live = connect(&server).await;

        let fill = live.execute(&buy("0.006")).await.unwrap();

        assert_eq!(fill.filled_qty, d("0.004"));
        assert_eq!(fill.filled_price, d("60010"));
        assert_eq!(fill.order_ref, Some(1001));
    }

    #[tokio::test]
    async fn an_order_that_ended_with_nothing_filled_is_a_plain_error() {
        let server = venue().await;
        on(&server, "POST", ORDER_PATH, ok(order("EXPIRED", "0", "0"))).await;
        let live = connect(&server).await;

        let err = live.execute(&buy("0.006")).await.unwrap_err();

        assert!(err.downcast_ref::<OrderStateUnknown>().is_none(), "{err:#}");
        assert!(err.to_string().contains("nothing filled"), "{err}");
    }

    #[tokio::test]
    async fn a_new_order_is_polled_by_client_order_id_until_it_fills() {
        let server = venue().await;
        on(&server, "POST", ORDER_PATH, ok(order("NEW", "0", "0"))).await;
        Mock::given(method("GET"))
            .and(path(ORDER_PATH))
            .respond_with(ok(order("PARTIALLY_FILLED", "0.002", "60000")))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        on(
            &server,
            "GET",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")),
        )
        .await;
        on(
            &server,
            "GET",
            USER_TRADES_PATH,
            ok(trades(&[("0.006", "0.144", "USDT")])),
        )
        .await;
        let live = connect(&server).await;

        let fill = live.execute(&buy("0.006")).await.unwrap();

        assert_eq!(fill.filled_qty, d("0.006"));
        let placed = requests_to(&server, "POST", ORDER_PATH).await;
        let polled = requests_to(&server, "GET", ORDER_PATH).await;
        assert_eq!(polled.len(), 2);
        assert_eq!(
            param(&polled[0], "origClientOrderId"),
            param(&placed[0], "newClientOrderId")
        );
    }

    #[tokio::test]
    async fn a_rejected_reduce_only_order_is_a_plain_error_carrying_the_code() {
        let server = venue().await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            venue_error(-2022, "ReduceOnly Order is rejected."),
        )
        .await;
        let live = connect(&server).await;
        let close = OrderRequest {
            side: OrderSide::Sell,
            reduce_only: true,
            ..buy("0.006")
        };

        let err = live.execute(&close).await.unwrap_err();

        assert!(err.downcast_ref::<OrderStateUnknown>().is_none(), "{err:#}");
        let message = err.to_string();
        assert!(message.contains("-2022"), "{message}");
        assert!(
            message.contains("ReduceOnly Order is rejected."),
            "{message}"
        );
        // A refusal needs no status query.
        assert!(requests_to(&server, "GET", ORDER_PATH).await.is_empty());
    }

    #[tokio::test]
    async fn commission_lines_that_arrive_late_are_waited_for() {
        let server = venue().await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")),
        )
        .await;
        // First read: only one of the two lines has been booked.
        Mock::given(method("GET"))
            .and(path(USER_TRADES_PATH))
            .respond_with(ok(trades(&[("0.004", "0.096", "USDT")])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        on(
            &server,
            "GET",
            USER_TRADES_PATH,
            ok(trades(&[
                ("0.004", "0.096", "USDT"),
                ("0.002", "0.048", "USDT"),
            ])),
        )
        .await;
        let live = connect(&server).await;

        let fill = live.execute(&buy("0.006")).await.unwrap();

        assert_eq!(fill.commission, d("0.144"));
        assert_eq!(requests_to(&server, "GET", USER_TRADES_PATH).await.len(), 2);
    }

    #[tokio::test]
    async fn commission_lines_that_never_arrive_are_order_state_unknown() {
        let server = venue().await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")),
        )
        .await;
        on(&server, "GET", USER_TRADES_PATH, ok(serde_json::json!([]))).await;
        let live = connect(&server).await;

        let err = live.execute(&buy("0.006")).await.unwrap_err();

        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown, never a fill with a guessed commission");
        assert_eq!(unknown.order_ref, Some(1001));
        assert_eq!(unknown.symbol, "BTCUSDT");
        assert!(
            format!("{err:#}").contains("commission is unknown"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn commission_in_two_assets_is_order_state_unknown() {
        let server = venue().await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")),
        )
        .await;
        on(
            &server,
            "GET",
            USER_TRADES_PATH,
            ok(trades(&[
                ("0.004", "0.0002", "BNB"),
                ("0.002", "0.048", "USDT"),
            ])),
        )
        .await;
        let live = connect(&server).await;

        let err = live.execute(&buy("0.006")).await.unwrap_err();

        assert_eq!(
            err.downcast_ref::<OrderStateUnknown>().unwrap().order_ref,
            Some(1001)
        );
        assert!(
            format!("{err:#}").contains("more than one asset"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_dropped_placing_call_is_recovered_by_the_client_order_id() {
        let server = venue().await;
        // The venue takes the order, but its answer outlives the request
        // timeout: the connection is dropped after the order was sent.
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")).set_delay(Duration::from_millis(600)),
        )
        .await;
        on(
            &server,
            "GET",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")),
        )
        .await;
        on(
            &server,
            "GET",
            USER_TRADES_PATH,
            ok(trades(&[("0.006", "0.144", "USDT")])),
        )
        .await;
        let live = connect(&server).await;

        let fill = live.execute(&buy("0.006")).await.unwrap();

        assert_eq!(fill.filled_qty, d("0.006"));
        assert_eq!(fill.order_ref, Some(1001));
        let placed = requests_to(&server, "POST", ORDER_PATH).await;
        let asked = requests_to(&server, "GET", ORDER_PATH).await;
        assert_eq!(placed.len(), 1, "a lost order is looked up, never resent");
        assert_eq!(
            param(&asked[0], "origClientOrderId"),
            param(&placed[0], "newClientOrderId")
        );
    }

    #[tokio::test]
    async fn a_dropped_placing_call_and_a_failed_query_is_order_state_unknown() {
        let server = venue().await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")).set_delay(Duration::from_millis(600)),
        )
        .await;
        on(&server, "GET", ORDER_PATH, ResponseTemplate::new(500)).await;
        let live = connect(&server).await;

        let err = live.execute(&buy("0.006")).await.unwrap_err();

        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .expect("an OrderStateUnknown");
        assert_eq!(unknown.symbol, "BTCUSDT");
        assert_eq!(unknown.order_ref, None);
        let placed = requests_to(&server, "POST", ORDER_PATH).await;
        assert_eq!(
            Some(unknown.client_order_id.clone()),
            param(&placed[0], "newClientOrderId")
        );
    }

    #[tokio::test]
    async fn no_such_order_once_recv_window_has_passed_is_a_plain_error() {
        let server = venue().await;
        on(&server, "POST", ORDER_PATH, ResponseTemplate::new(502)).await;
        on(
            &server,
            "GET",
            ORDER_PATH,
            venue_error(-2013, "Order does not exist."),
        )
        .await;
        let live = connect(&server).await;

        let started = Instant::now();
        let err = live.execute(&buy("0.006")).await.unwrap_err();

        assert!(err.downcast_ref::<OrderStateUnknown>().is_none(), "{err:#}");
        assert!(err.to_string().contains("never accepted"), "{err}");
        assert!(started.elapsed() >= fast().recv_window);
    }

    #[tokio::test]
    async fn a_minus_1021_on_the_order_retries_it_once_with_a_fresh_clock() {
        let server = venue().await;
        Mock::given(method("POST"))
            .and(path(ORDER_PATH))
            .respond_with(venue_error(
                -1021,
                "Timestamp for this request is outside of the recvWindow.",
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        on(
            &server,
            "POST",
            ORDER_PATH,
            ok(order("FILLED", "0.006", "60000")),
        )
        .await;
        on(
            &server,
            "GET",
            USER_TRADES_PATH,
            ok(trades(&[("0.006", "0.144", "USDT")])),
        )
        .await;
        let live = connect(&server).await;

        let fill = live.execute(&buy("0.006")).await.unwrap();

        assert_eq!(fill.order_ref, Some(1001));
        let placed = requests_to(&server, "POST", ORDER_PATH).await;
        assert_eq!(placed.len(), 2);
        assert_eq!(
            param(&placed[0], "newClientOrderId"),
            param(&placed[1], "newClientOrderId")
        );
        // Read at connect, and again after the -1021.
        assert_eq!(requests_to(&server, "GET", "/fapi/v1/time").await.len(), 2);
    }

    #[tokio::test]
    async fn the_clock_is_read_again_once_the_refresh_interval_has_passed() {
        let server = filling_venue().await;
        let timings = CexTimings {
            clock_refresh: Duration::ZERO,
            ..fast()
        };
        let live = BinanceFuturesLive::connect_with(rest(&server, timings), &["BTCUSDT"])
            .await
            .unwrap();
        live.execute(&buy("0.006")).await.unwrap();

        // The explicit check at connect, then one before each of the five
        // signed calls.
        assert_eq!(requests_to(&server, "GET", "/fapi/v1/time").await.len(), 6);
    }

    #[tokio::test]
    async fn satisfies_the_executor_contract_against_a_mocked_venue() {
        let server = filling_venue().await;
        let live = connect(&server).await;
        cex_executor_contract(
            &live,
            CexContractFixture {
                request: buy("0.006"),
            },
        )
        .await;
    }

    // ---- Gated runs against the real testnet --------------------------
    //
    // No-ops unless BINANCE_FUTURES_API_KEY and BINANCE_FUTURES_API_SECRET
    // are set (and BINANCE_FUTURES_BASE_URL, if the default testnet host is
    // not the right one). The ones that place many orders also need
    // BINANCE_FUTURES_RUN_ACCEPTANCE=1. BINANCE_FUTURES_TEST_SYMBOL picks
    // the symbol (BTCUSDT by default). None of them trades on top of an
    // existing position: each refuses to start unless the symbol is flat,
    // and closes what it opened.

    fn testnet() -> Option<BinanceFuturesConfig> {
        BinanceFuturesConfig::from_env().ok()
    }

    fn acceptance_opted_in() -> bool {
        std::env::var("BINANCE_FUTURES_RUN_ACCEPTANCE").as_deref() == Ok("1")
    }

    pub(crate) fn testnet_symbol() -> String {
        std::env::var("BINANCE_FUTURES_TEST_SYMBOL").unwrap_or_else(|_| "BTCUSDT".to_string())
    }

    pub(crate) async fn mark_price(live: &BinanceFuturesLive, symbol: &str) -> Decimal {
        #[derive(Deserialize)]
        struct PremiumIndex {
            #[serde(rename = "markPrice")]
            mark_price: Decimal,
        }
        let index: PremiumIndex = live
            .rest
            .client()
            .public_get("/fapi/v1/premiumIndex", &[("symbol", symbol.to_string())])
            .await
            .unwrap();
        index.mark_price
    }

    /// The venue's position in `symbol`, read directly so these runs do not
    /// depend on the account reads they may be checking.
    async fn position_qty(live: &BinanceFuturesLive, symbol: &str) -> Decimal {
        #[derive(Deserialize)]
        struct Row {
            symbol: String,
            #[serde(rename = "positionAmt")]
            position_amt: Decimal,
        }
        let rows: Vec<Row> = live
            .rest
            .client()
            .signed(
                Method::GET,
                "/fapi/v3/positionRisk",
                &[("symbol", symbol.to_string())],
            )
            .await
            .unwrap();
        let rows: Vec<Row> = rows.into_iter().filter(|r| r.symbol == symbol).collect();
        assert!(rows.len() <= 1, "more than one position row for {symbol}");
        rows.first().map_or(Decimal::ZERO, |row| row.position_amt)
    }

    /// A quantity comfortably above both minimums at `price`, plus
    /// `extra_steps` steps so that each order of a run has its own size.
    pub(crate) fn tradable(filters: &SymbolFilters, price: Decimal, extra_steps: u32) -> Decimal {
        let for_notional = (filters.min_notional * d("1.5") / price / filters.step_size).ceil()
            * filters.step_size;
        for_notional.max(filters.min_qty) + filters.step_size * Decimal::from(extra_steps)
    }

    fn order_for(symbol: &str, side: OrderSide, quantity: Decimal, price: Decimal) -> OrderRequest {
        OrderRequest {
            symbol: symbol.to_string(),
            side,
            quantity,
            quoted_price: price,
            reduce_only: false,
        }
    }

    /// Closes whatever position `symbol` holds with a reduce-only order.
    pub(crate) async fn flatten(live: &BinanceFuturesLive, symbol: &str, price: Decimal) {
        let qty = position_qty(live, symbol).await;
        if qty.is_zero() {
            return;
        }
        let side = if qty > Decimal::ZERO {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        };
        let close = OrderRequest {
            reduce_only: true,
            ..order_for(symbol, side, qty.abs(), price)
        };
        live.execute(&close)
            .await
            .unwrap_or_else(|err| panic!("closing the test position of {qty}: {err:#}"));
        assert!(position_qty(live, symbol).await.is_zero());
    }

    /// Checks a fill against the venue's own trade lines: quantity and
    /// commission exactly, value to the cent.
    async fn reconcile(live: &BinanceFuturesLive, symbol: &str, fill: &CexFill) {
        #[derive(Deserialize)]
        struct Trade {
            qty: Decimal,
            #[serde(rename = "quoteQty")]
            quote_qty: Decimal,
            commission: Decimal,
            #[serde(rename = "commissionAsset")]
            commission_asset: String,
        }
        assert_eq!(fill.provenance, Provenance::Landed);
        let order_id = fill.order_ref.expect("a landed fill carries its order id");
        let lines: Vec<Trade> = live
            .rest
            .client()
            .signed(
                Method::GET,
                USER_TRADES_PATH,
                &[
                    ("symbol", symbol.to_string()),
                    ("orderId", order_id.to_string()),
                ],
            )
            .await
            .unwrap();
        let qty: Decimal = lines.iter().map(|line| line.qty).sum();
        let quote: Decimal = lines.iter().map(|line| line.quote_qty).sum();
        let commission: Decimal = lines.iter().map(|line| line.commission).sum();
        assert_eq!(qty, fill.filled_qty, "order {order_id}: quantity");
        assert_eq!(commission, fill.commission, "order {order_id}: commission");
        assert!(lines
            .iter()
            .all(|line| line.commission_asset == fill.commission_asset));
        let value = fill.filled_qty * fill.filled_price;
        assert!(
            (quote - value).abs() <= d("0.01"),
            "order {order_id}: the trades are worth {quote}, the fill {value}"
        );
    }

    async fn connect_to_testnet(config: BinanceFuturesConfig, symbol: &str) -> BinanceFuturesLive {
        let live = BinanceFuturesLive::connect(config, &[symbol])
            .await
            .unwrap_or_else(|err| panic!("connecting to the testnet: {err:#}"));
        assert!(
            position_qty(&live, symbol).await.is_zero(),
            "refusing to trade on top of an existing {symbol} position on the testnet"
        );
        live
    }

    #[tokio::test]
    async fn testnet_satisfies_the_executor_contract() {
        let Some(config) = testnet() else {
            eprintln!("skipping: BINANCE_FUTURES_API_KEY / BINANCE_FUTURES_API_SECRET not set");
            return;
        };
        let symbol = testnet_symbol();
        let live = connect_to_testnet(config, &symbol).await;
        let price = mark_price(&live, &symbol).await;
        let quantity = tradable(&live.filters[&symbol], price, 0);

        cex_executor_contract(
            &live,
            CexContractFixture {
                request: order_for(&symbol, OrderSide::Buy, quantity, price),
            },
        )
        .await;
        flatten(&live, &symbol, price).await;
    }

    /// `SPEC.md` §9.2 on the testnet: 100 orders of distinct sizes, each
    /// reconciled to the cent against `userTrades`, then one injected
    /// failure per variant. Its output records what V2 asks to confirm.
    #[tokio::test]
    async fn testnet_acceptance_one_hundred_orders_and_each_failure() {
        let Some(config) = testnet() else {
            eprintln!("skipping: BINANCE_FUTURES_API_KEY / BINANCE_FUTURES_API_SECRET not set");
            return;
        };
        if !acceptance_opted_in() {
            eprintln!("skipping: set BINANCE_FUTURES_RUN_ACCEPTANCE=1 to place the 100-order run");
            return;
        }
        let symbol = testnet_symbol();
        let live = connect_to_testnet(config, &symbol).await;
        let filters = live.filters[&symbol].clone();
        let price = mark_price(&live, &symbol).await;

        for i in 0..100u32 {
            let side = if i % 2 == 0 {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            };
            let request = order_for(&symbol, side, tradable(&filters, price, i), price);
            let fill = live
                .execute(&request)
                .await
                .unwrap_or_else(|err| panic!("order {i}: {err:#}"));
            reconcile(&live, &symbol, &fill).await;
        }
        flatten(&live, &symbol, price).await;
        eprintln!("100 orders filled and reconciled to the cent");

        // A rejected reduce-only order: the position is flat.
        let err = live
            .execute(&OrderRequest {
                reduce_only: true,
                ..order_for(
                    &symbol,
                    OrderSide::Sell,
                    tradable(&filters, price, 0),
                    price,
                )
            })
            .await
            .expect_err("a reduce-only order against a flat position must be refused");
        assert!(err.downcast_ref::<OrderStateUnknown>().is_none(), "{err:#}");
        eprintln!("reduce-only against a flat position: {err:#}");

        // A notional refusal before sending.
        let err = live
            .execute(&order_for(
                &symbol,
                OrderSide::Buy,
                filters.min_qty,
                d("0.00000001"),
            ))
            .await
            .expect_err("an order below the notional minimum must be refused");
        assert!(err.to_string().contains("notional"), "{err:#}");

        // A notional refusal by the venue: an estimate that overstates the
        // price gets past this adapter's check.
        if filters.min_qty * price < filters.min_notional {
            let overstated = filters.min_notional / filters.min_qty * d("2");
            let err = live
                .execute(&order_for(
                    &symbol,
                    OrderSide::Buy,
                    filters.min_qty,
                    overstated,
                ))
                .await
                .expect_err("the venue must refuse an order below its notional minimum");
            assert!(err.downcast_ref::<OrderStateUnknown>().is_none(), "{err:#}");
            eprintln!("venue's notional refusal: {err:#}");
        } else {
            eprintln!("no venue notional refusal: {symbol}'s minimum quantity clears the notional");
        }
        flatten(&live, &symbol, price).await;

        // OrderStateUnknown, forced by dropping every answer after sending.
        let rushed = BinanceFuturesLive {
            rest: Arc::new(live.rest.with_other_timings(CexTimings {
                request_timeout: Duration::from_millis(1),
                ..live.rest.client().timings().clone()
            })),
            filters: live.filters.clone(),
        };
        let request = order_for(&symbol, OrderSide::Buy, tradable(&filters, price, 0), price);
        let err = rushed
            .execute(&request)
            .await
            .expect_err("every answer was dropped");
        let unknown = err
            .downcast_ref::<OrderStateUnknown>()
            .unwrap_or_else(|| panic!("expected OrderStateUnknown, got {err:#}"));
        // Find out, as a caller must, before acting on the symbol again.
        let found = live
            .rest
            .client()
            .signed::<FuturesOrder>(
                Method::GET,
                ORDER_PATH,
                &[
                    ("symbol", symbol.clone()),
                    ("origClientOrderId", unknown.client_order_id.clone()),
                ],
            )
            .await;
        eprintln!(
            "forced OrderStateUnknown for {}: the venue {}",
            unknown.client_order_id,
            match &found {
                Ok(order) => format!(
                    "has it as order {} ({}, {} filled)",
                    order.order_id, order.status, order.executed_qty
                ),
                Err(err) => format!("does not have it: {err}"),
            }
        );
        flatten(&live, &symbol, price).await;

        // What the venue does with a reduce-only order larger than the
        // position (V1 and V2 leave this open; CexStub copies the answer).
        let opened = live
            .execute(&order_for(
                &symbol,
                OrderSide::Buy,
                tradable(&filters, price, 0),
                price,
            ))
            .await
            .unwrap();
        let oversized = OrderRequest {
            reduce_only: true,
            ..order_for(&symbol, OrderSide::Sell, opened.filled_qty * d("2"), price)
        };
        match live.execute(&oversized).await {
            Ok(fill) => eprintln!(
                "RECORD: a reduce-only sell of {} against a long of {} FILLED {} (the rest \
                 expired) — CexStub must fill up to the position",
                oversized.quantity, opened.filled_qty, fill.filled_qty
            ),
            Err(err) => eprintln!(
                "RECORD: a reduce-only sell of {} against a long of {} was REJECTED: {err:#} — \
                 CexStub's rejection matches",
                oversized.quantity, opened.filled_qty
            ),
        }
        flatten(&live, &symbol, price).await;
    }
}
