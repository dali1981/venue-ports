//! What Binance's spot testnet said on 7 October 2026, read by the code that reads the venue.
//!
//! `fixtures/binance-spot/testnet/2026-10-07/` is a recording the consumer made with one account:
//! seventeen replies, each as a `.json` body and a `.status`, the account's `uid` the only edit
//! (`REDACTED-UID`). Origin `testnet` in `docs/responses/binance-spot.md`. Each test serves a body
//! with the status its own `.status` file holds and asserts what the code reads from it; the
//! expected values are the body's figures.
//!
//! These tests were added beside the `documented` ones and change none of them: the documented
//! five-trade order, the LIMIT order still working and the two refusals the recording has no
//! counterpart for keep their rows.

use crate::balance::SpotBalanceReader;
use crate::cex::binance::clock::local_now_ms;
use crate::cex::binance::rest::{BinanceConfig, BinanceRest};
use crate::cex::binance::{
    BinanceLive, CommissionDiscount, CommissionRates, MakerTaker, SymbolRules,
};
use crate::cex::{
    refusal_of, CexExecutor, CexOrders, CexTimings, CexTrade, OrderRequest, OrderSide, OrderState,
};
use crate::Provenance;
use anyhow::{bail, Result};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EXCHANGE_INFO: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/00-exchange-info.json");
const TICKER_PRICE: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/00-ticker-price.json");
const ACCOUNT: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/01-account.json");
const ACCOUNT_COMMISSION: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/02-account-commission.json");
const ORDER_TEST_BUY: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/03-order-test-buy.json");
const ORDER_TEST_SELL: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/03-order-test-sell.json");
const ORDER_MARKET_BUY: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/04-order-market-buy.json");
const ORDER_MARKET_SELL: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/04-order-market-sell.json");
const MY_TRADES_BUY: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-my-trades-buy.json");
const MY_TRADES_SELL: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-my-trades-sell.json");
const ORDER_QUERY_BUY: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-order-query-buy.json");
const ORDER_QUERY_SELL: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-order-query-sell.json");
const LOT_SIZE_BELOW_MIN: &str = include_str!(
    "../../../fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-below-min.json"
);
const LOT_SIZE_OFF_STEP: &str = include_str!(
    "../../../fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-off-step.json"
);
const MARKET_LOT_SIZE: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/06-order-market-lot-size.json");
const NOTIONAL: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/06-order-notional.json");
const INSUFFICIENT_BALANCE: &str = include_str!(
    "../../../fixtures/binance-spot/testnet/2026-10-07/07-order-insufficient-balance.json"
);

const EXCHANGE_INFO_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/00-exchange-info.status");
const TICKER_PRICE_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/00-ticker-price.status");
const ACCOUNT_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/01-account.status");
const ACCOUNT_COMMISSION_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/02-account-commission.status");
const ORDER_TEST_BUY_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/03-order-test-buy.status");
const ORDER_TEST_SELL_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/03-order-test-sell.status");
const ORDER_MARKET_BUY_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/04-order-market-buy.status");
const ORDER_MARKET_SELL_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/04-order-market-sell.status");
const MY_TRADES_BUY_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-my-trades-buy.status");
const MY_TRADES_SELL_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-my-trades-sell.status");
const ORDER_QUERY_BUY_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-order-query-buy.status");
const ORDER_QUERY_SELL_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/05-order-query-sell.status");
const LOT_SIZE_BELOW_MIN_STATUS: &str = include_str!(
    "../../../fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-below-min.status"
);
const LOT_SIZE_OFF_STEP_STATUS: &str = include_str!(
    "../../../fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-off-step.status"
);
const MARKET_LOT_SIZE_STATUS: &str = include_str!(
    "../../../fixtures/binance-spot/testnet/2026-10-07/06-order-market-lot-size.status"
);
const NOTIONAL_STATUS: &str =
    include_str!("../../../fixtures/binance-spot/testnet/2026-10-07/06-order-notional.status");
const INSUFFICIENT_BALANCE_STATUS: &str = include_str!(
    "../../../fixtures/binance-spot/testnet/2026-10-07/07-order-insufficient-balance.status"
);

/// Every recorded reply with its status: the body, then the `.status` file's text.
const RECORDED: [(&str, &str, &str); 17] = [
    ("00-exchange-info", EXCHANGE_INFO, EXCHANGE_INFO_STATUS),
    ("00-ticker-price", TICKER_PRICE, TICKER_PRICE_STATUS),
    ("01-account", ACCOUNT, ACCOUNT_STATUS),
    (
        "02-account-commission",
        ACCOUNT_COMMISSION,
        ACCOUNT_COMMISSION_STATUS,
    ),
    ("03-order-test-buy", ORDER_TEST_BUY, ORDER_TEST_BUY_STATUS),
    (
        "03-order-test-sell",
        ORDER_TEST_SELL,
        ORDER_TEST_SELL_STATUS,
    ),
    (
        "04-order-market-buy",
        ORDER_MARKET_BUY,
        ORDER_MARKET_BUY_STATUS,
    ),
    (
        "04-order-market-sell",
        ORDER_MARKET_SELL,
        ORDER_MARKET_SELL_STATUS,
    ),
    ("05-my-trades-buy", MY_TRADES_BUY, MY_TRADES_BUY_STATUS),
    ("05-my-trades-sell", MY_TRADES_SELL, MY_TRADES_SELL_STATUS),
    (
        "05-order-query-buy",
        ORDER_QUERY_BUY,
        ORDER_QUERY_BUY_STATUS,
    ),
    (
        "05-order-query-sell",
        ORDER_QUERY_SELL,
        ORDER_QUERY_SELL_STATUS,
    ),
    (
        "06-order-lot-size-below-min",
        LOT_SIZE_BELOW_MIN,
        LOT_SIZE_BELOW_MIN_STATUS,
    ),
    (
        "06-order-lot-size-off-step",
        LOT_SIZE_OFF_STEP,
        LOT_SIZE_OFF_STEP_STATUS,
    ),
    (
        "06-order-market-lot-size",
        MARKET_LOT_SIZE,
        MARKET_LOT_SIZE_STATUS,
    ),
    ("06-order-notional", NOTIONAL, NOTIONAL_STATUS),
    (
        "07-order-insufficient-balance",
        INSUFFICIENT_BALANCE,
        INSUFFICIENT_BALANCE_STATUS,
    ),
];

fn decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

/// The recorded reply served as it was: its body, with the status its `.status` file holds.
fn served(body: &str, status: &str) -> ResponseTemplate {
    let status: u16 = status
        .trim()
        .parse()
        .expect("a status file holds an HTTP status");
    ResponseTemplate::new(status).set_body_raw(body, "application/json")
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

fn rest(server: &MockServer) -> BinanceRest {
    BinanceRest::with_timings(
        BinanceConfig {
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            api_secret: "test-secret".to_string(),
        },
        fast(),
    )
}

/// AEROUSDT trades in steps of 0.1 (`00-exchange-info.json`'s `LOT_SIZE`).
fn live(server: &MockServer) -> BinanceLive {
    BinanceLive::new(
        rest(server),
        HashMap::from([("AEROUSDT".to_string(), decimal("0.1"))]),
    )
}

/// A venue with a clock. Every signed call reads it first.
async fn venue() -> MockServer {
    let server = MockServer::start().await;
    answering(
        &server,
        "GET",
        "/api/v3/time",
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "serverTime": local_now_ms() })),
    )
    .await;
    server
}

async fn answering(server: &MockServer, verb: &str, route: &str, response: ResponseTemplate) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(response)
        .mount(server)
        .await;
}

fn order(side: OrderSide) -> OrderRequest {
    OrderRequest {
        symbol: "AEROUSDT".to_string(),
        side,
        quantity: decimal("10"),
        quoted_price: decimal("0.79"),
        reduce_only: false,
    }
}

#[tokio::test]
async fn the_recorded_exchange_info_reads_as_the_rules_of_aerousdt() -> Result<()> {
    let server = venue().await;
    answering(
        &server,
        "GET",
        "/api/v3/exchangeInfo",
        served(EXCHANGE_INFO, EXCHANGE_INFO_STATUS),
    )
    .await;

    let rules = rest(&server).symbol_rules("AEROUSDT").await?;

    // `LOT_SIZE` 0.1 / 0.1 and `NOTIONAL` 5, applied to market orders, in the body.
    assert_eq!(
        rules,
        SymbolRules {
            symbol: "AEROUSDT".to_string(),
            status: "TRADING".to_string(),
            base_asset: "AERO".to_string(),
            quote_asset: "USDT".to_string(),
            lot_step: decimal("0.1"),
            min_qty: decimal("0.1"),
            min_notional: Some(decimal("5")),
            apply_min_to_market: Some(true),
        }
    );
    Ok(())
}

/// No typed read of `/ticker/price` exists in this crate; the body is the shape LV1 and
/// the consumer's feed depend on: a symbol and a price, a decimal in a string.
#[test]
fn the_recorded_ticker_price_is_a_symbol_and_a_decimal_in_a_string() {
    let body: serde_json::Value = serde_json::from_str(TICKER_PRICE).unwrap();
    assert_eq!(body["symbol"], "AEROUSDT");
    assert_eq!(decimal(body["price"].as_str().unwrap()), decimal("0.7904"));
}

#[tokio::test]
async fn the_recorded_account_reads_as_exact_balances_whatever_the_assets_are_called() -> Result<()>
{
    let server = venue().await;
    answering(
        &server,
        "GET",
        "/api/v3/account",
        served(ACCOUNT, ACCOUNT_STATUS),
    )
    .await;

    let account = rest(&server).balances().await?;

    // The testnet faucet's account: every one of its 517 assets is held.
    assert_eq!(account.balances.len(), 517);
    let usdt = account.balance("USDT").expect("USDT is held");
    assert_eq!((usdt.free, usdt.locked), (decimal("10000"), Decimal::ZERO));
    assert_eq!(usdt.free.to_string(), "10000.00000000", "the venue's scale");
    assert_eq!(account.balance("AERO").unwrap().free, decimal("633"));
    // Asset names that are not ASCII are read as they are.
    for name in ["这是测试币", "币安人生", "牛来"] {
        assert!(account.balance(name).is_some(), "{name}");
    }
    assert_eq!(account.update_time_ms, Some(1_791_367_242_545));
    // The one edit of a recording is the `uid`.
    assert!(ACCOUNT.contains("\"uid\":\"REDACTED-UID\""));
    Ok(())
}

#[tokio::test]
async fn the_recorded_commission_reads_with_a_null_discount_asset() -> Result<()> {
    let server = venue().await;
    answering(
        &server,
        "GET",
        "/api/v3/account/commission",
        served(ACCOUNT_COMMISSION, ACCOUNT_COMMISSION_STATUS),
    )
    .await;

    let commission = rest(&server).account_commission("AEROUSDT").await?;

    assert_eq!(commission.symbol, "AEROUSDT");
    for rates in [&commission.standard, &commission.special, &commission.tax] {
        assert_eq!(
            [rates.maker, rates.taker, rates.buyer, rates.seller],
            [Decimal::ZERO; 4]
        );
    }
    // The venue names no discount asset: `"discountAsset": null`, a rate of zero.
    assert_eq!(
        commission.discount,
        CommissionDiscount {
            enabled_for_account: true,
            enabled_for_symbol: true,
            asset: None,
            rate: Decimal::ZERO,
        }
    );
    Ok(())
}

#[tokio::test]
async fn the_recorded_order_tests_read_as_the_commission_rates_of_an_order() -> Result<()> {
    for (side, body, status) in [
        ("BUY", ORDER_TEST_BUY, ORDER_TEST_BUY_STATUS),
        ("SELL", ORDER_TEST_SELL, ORDER_TEST_SELL_STATUS),
    ] {
        let server = venue().await;
        answering(&server, "POST", "/api/v3/order/test", served(body, status)).await;

        let rates = rest(&server)
            .test_market_order("AEROUSDT", side, decimal("10"), "r2-test")
            .await?;

        let none = MakerTaker {
            maker: Decimal::ZERO,
            taker: Decimal::ZERO,
        };
        assert_eq!(
            rates,
            CommissionRates {
                standard: none.clone(),
                special: Some(none.clone()),
                tax: none,
                discount: Some(CommissionDiscount {
                    enabled_for_account: true,
                    enabled_for_symbol: true,
                    asset: None,
                    rate: Decimal::ZERO,
                }),
            },
            "{side}"
        );
    }
    Ok(())
}

/// A market order answered `FULL` with one trade, as `execute` reads it: the fill's
/// price is the trade's, the commission is the trade's (zero, on the testnet), and
/// the order's id and time are the venue's.
#[tokio::test]
async fn the_recorded_market_orders_fill_as_execute_reads_them() -> Result<()> {
    for (side, body, status, price, commission_asset, order_id, time, trade_id) in [
        (
            OrderSide::Buy,
            ORDER_MARKET_BUY,
            ORDER_MARKET_BUY_STATUS,
            "0.7915",
            "AERO",
            18_636,
            1_791_395_227_886,
            356,
        ),
        (
            OrderSide::Sell,
            ORDER_MARKET_SELL,
            ORDER_MARKET_SELL_STATUS,
            "0.7913",
            "USDT",
            18_637,
            1_791_395_229_270,
            357,
        ),
    ] {
        let server = venue().await;
        answering(&server, "POST", "/api/v3/order", served(body, status)).await;

        let fill = live(&server).execute(&order(side)).await?;

        assert_eq!(fill.filled_qty, decimal("10"));
        assert_eq!(fill.filled_price, decimal(price));
        assert_eq!(fill.commission, Decimal::ZERO);
        assert_eq!(fill.commission_asset, commission_asset);
        assert_eq!(fill.provenance, Provenance::Landed);
        assert_eq!(fill.order_ref, Some(order_id));
        assert_eq!(fill.venue_time_ms, Some(time));
        assert_eq!(
            fill.trades,
            [CexTrade {
                trade_id: Some(trade_id),
                price: decimal(price),
                qty: decimal("10"),
                commission: Decimal::ZERO,
                commission_asset: commission_asset.to_string(),
            }]
        );
    }
    Ok(())
}

/// The status query of a filled order lists no fills; they come from `myTrades`.
#[tokio::test]
async fn the_recorded_order_queries_read_as_filled_with_their_trade_lines() -> Result<()> {
    for (client_order_id, query, query_status, trades, trades_status, price, asset, id, trade_id) in [
        (
            "r2rec-20261007T174659-buy",
            ORDER_QUERY_BUY,
            ORDER_QUERY_BUY_STATUS,
            MY_TRADES_BUY,
            MY_TRADES_BUY_STATUS,
            "0.7915",
            "AERO",
            18_636,
            356,
        ),
        (
            "r2rec-20261007T174659-sell",
            ORDER_QUERY_SELL,
            ORDER_QUERY_SELL_STATUS,
            MY_TRADES_SELL,
            MY_TRADES_SELL_STATUS,
            "0.7913",
            "USDT",
            18_637,
            357,
        ),
    ] {
        let server = venue().await;
        answering(&server, "GET", "/api/v3/order", served(query, query_status)).await;
        answering(
            &server,
            "GET",
            "/api/v3/myTrades",
            served(trades, trades_status),
        )
        .await;

        let OrderState::Filled(fill) = live(&server)
            .order_state("AEROUSDT", client_order_id)
            .await?
        else {
            bail!("a FILLED order is Filled");
        };

        assert_eq!(fill.filled_qty, decimal("10"));
        assert_eq!(fill.filled_price, decimal(price));
        assert_eq!(fill.commission_asset, asset);
        assert_eq!(fill.order_ref, Some(id));
        assert_eq!(fill.client_order_id.as_deref(), Some(client_order_id));
        assert_eq!(
            fill.trades.iter().map(|t| t.trade_id).collect::<Vec<_>>(),
            [Some(trade_id)]
        );
    }
    Ok(())
}

/// Each refusal is a `VenueRefusal` carrying the venue's own code, its message and the
/// HTTP status it was served with (`400`).
#[tokio::test]
async fn the_recorded_refusals_are_venue_refusals_carrying_the_venues_code() -> Result<()> {
    for (body, status, code, msg) in [
        (
            LOT_SIZE_BELOW_MIN,
            LOT_SIZE_BELOW_MIN_STATUS,
            -1013,
            "Filter failure: LOT_SIZE",
        ),
        (
            LOT_SIZE_OFF_STEP,
            LOT_SIZE_OFF_STEP_STATUS,
            -1013,
            "Filter failure: LOT_SIZE",
        ),
        (
            MARKET_LOT_SIZE,
            MARKET_LOT_SIZE_STATUS,
            -1013,
            "Filter failure: LOT_SIZE",
        ),
        (NOTIONAL, NOTIONAL_STATUS, -1013, "Filter failure: NOTIONAL"),
        (
            INSUFFICIENT_BALANCE,
            INSUFFICIENT_BALANCE_STATUS,
            -2010,
            "Account has insufficient balance for requested action.",
        ),
    ] {
        let server = venue().await;
        answering(&server, "POST", "/api/v3/order", served(body, status)).await;

        let err = live(&server)
            .execute(&order(OrderSide::Buy))
            .await
            .unwrap_err();

        let refusal = refusal_of(&err).unwrap_or_else(|| panic!("{msg}: not a refusal: {err:#}"));
        assert_eq!(
            (refusal.status, refusal.code, refusal.msg.as_str()),
            (400, Some(code), msg)
        );
    }
    Ok(())
}

/// A status file holds the HTTP status and a newline; the bodies that were refused carry
/// `400` and a code, the others `200`. The other tests serve each body with its own.
#[test]
fn every_recorded_reply_carries_the_status_its_test_serves() {
    assert_eq!(RECORDED.len(), 17);
    for (name, body, status) in RECORDED {
        assert!(
            status.ends_with('\n') && status.trim().parse::<u16>().is_ok(),
            "{name}"
        );
        let refused = name.starts_with("06-") || name.starts_with("07-");
        assert_eq!(status.trim(), if refused { "400" } else { "200" }, "{name}");
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            json.get("code")
                .is_some_and(|code| code.as_i64().unwrap() < 0),
            refused,
            "{name}: a refusal is a body with a negative code"
        );
    }
}
